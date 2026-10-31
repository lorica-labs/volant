// SPDX-License-Identifier: GPL-3.0-or-later
//! Filters, tests and lookups that only ansible-core can run, called in the controller's Python
//! helper.
//!
//! One helper serves the whole run, started on the first call. The helper renders nothing a play
//! holds: the arguments and variables arrive as data, already rendered here.
//!
//! Action plugins run in helpers of their own, one per action running at that moment. While a
//! plugin runs, its helper may ask the controller to act on the host, and waits for the reply.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use serde_json::{Map, Value, json};
use volant_protocol::frame::MAX_FRAME_LEN;

use crate::python::{ActionHelper, PythonBuilder};

mod actions;
pub use actions::ActionCall;
mod connections;
mod conversation;
mod persistent;
mod wire;

use conversation::{answer, defaults_shape};
pub use wire::{Ask, BridgeError, PluginKind, Reply, TmpOp};
pub(crate) use wire::{remember_chain, split_failure};

pub(crate) type Start = Box<dyn Fn() -> anyhow::Result<PythonBuilder> + Send + Sync>;

enum State {
    Idle,
    Running(PythonBuilder),
    /// The helper is gone; every later call fails with this and nothing restarts it.
    Dead(BridgeError),
}

/// The helpers that run action plugins, apart from the one that runs filters, tests and lookups:
/// an action that renders a template through that one never waits behind itself.
// One helper per action running at once, so as many as the run's forks; cap the pool
// if their memory shows up.
#[derive(Default)]
struct Actions {
    idle: Vec<ActionHelper>,
    /// Why no helper could start; every later action fails with it and nothing retries.
    broken: Option<BridgeError>,
    next: u64,
}

impl Drop for Actions {
    /// Every idle helper is asked to stop before any is waited for, so the run's exit waits
    /// for the slowest of them rather than for each in turn.
    fn drop(&mut self) {
        for helper in &mut self.idle {
            helper.close_input();
        }
    }
}

/// The run's channel to the Python helper.
pub struct Bridge {
    start: Start,
    state: Mutex<(State, u64)>,
    actions: Mutex<Actions>,
    /// The plugin directories of a bridge made by [`Bridge::late`].
    dirs: Arc<Mutex<Vec<PathBuf>>>,
    /// What each lookup reads of the play's variables, asked of the helper once per name.
    needs: Mutex<HashMap<String, LookupNeeds>>,
    /// The persistent connection socket of each host, see `persistent`.
    sockets: Mutex<HashMap<String, String>>,
}

/// The variables a lookup plugin reads: the names it asks for, or all of them when it
/// enumerates or hands them on.
#[derive(Debug, Clone, Default)]
pub struct LookupNeeds {
    pub names: Vec<String>,
    pub all: bool,
}

impl fmt::Debug for Bridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Bridge")
    }
}

impl Bridge {
    /// [`Bridge::new`] for a bridge the inventory uses before the playbooks, and so their plugin
    /// directories, are known: [`Bridge::set_plugin_dirs`] gives them later.
    pub fn late() -> Arc<Bridge> {
        let dirs = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let seen = Arc::clone(&dirs);
        Arc::new(Bridge {
            start: Box::new(move || {
                let mut builder = PythonBuilder::start()?;
                let dirs = seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                builder.add_plugin_dirs(&dirs)?;
                Ok(builder)
            }),
            state: Mutex::new((State::Idle, 0)),
            actions: Mutex::new(Actions::default()),
            dirs,
            needs: Mutex::default(),
            sockets: Mutex::default(),
        })
    }

    /// The plugin directories of a [`Bridge::late`] bridge: used by the helper it starts, or added
    /// to the one already running.
    ///
    /// # Errors
    /// Why the running helper refused the directories.
    pub fn set_plugin_dirs(&self, dirs: Vec<PathBuf>) -> anyhow::Result<()> {
        self.dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone_from(&dirs);
        if let State::Running(helper) = &mut self.lock().0 {
            helper.add_plugin_dirs(&dirs)?;
        }
        Ok(())
    }

    /// Starts nothing: the helper starts under the first call.
    pub fn new(plugin_dirs: Vec<PathBuf>) -> Arc<Bridge> {
        Bridge::with_start(Box::new(move || {
            let mut builder = PythonBuilder::start()?;
            builder.add_plugin_dirs(&plugin_dirs)?;
            Ok(builder)
        }))
    }

    /// [`Bridge::new`] under the interpreter `python` rather than the first one with ansible-core.
    pub fn under(python: PathBuf, plugin_dirs: Vec<PathBuf>) -> Arc<Bridge> {
        Bridge::with_start(Box::new(move || {
            let python = python.to_string_lossy();
            let mut builder = PythonBuilder::under(&python)?;
            builder.add_plugin_dirs(&plugin_dirs)?;
            Ok(builder)
        }))
    }

    pub(crate) fn with_start(start: Start) -> Arc<Bridge> {
        Arc::new(Bridge {
            start,
            state: Mutex::new((State::Idle, 0)),
            actions: Mutex::new(Actions::default()),
            dirs: Arc::default(),
            needs: Mutex::default(),
            sockets: Mutex::default(),
        })
    }

    /// Serves the calls from a helper the run already started for its payloads. It must have
    /// been given the same plugin directories; a bridge that has started its own keeps it.
    pub fn adopt(&self, helper: PythonBuilder) {
        let mut guard = self.lock();
        if matches!(guard.0, State::Idle) {
            guard.0 = State::Running(helper);
        }
    }

    /// Starts the helper on a thread of its own, so the run's first call does not wait for the
    /// whole start. A call made meanwhile waits for that start and never makes another.
    pub fn warm(self: &Arc<Self>) {
        let bridge = Arc::clone(self);
        std::thread::spawn(move || {
            let mut guard = bridge.lock();
            if matches!(guard.0, State::Idle) {
                guard.0 = bridge.started();
            }
        });
    }

    fn started(&self) -> State {
        match (self.start)() {
            Ok(helper) => State::Running(helper),
            Err(err) => State::Dead(BridgeError::Start(format!("{err:#}"))),
        }
    }

    fn lock(&self) -> MutexGuard<'_, (State, u64)> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs one plugin. `args` are the positional arguments (the value a filter or test is
    /// given comes first), `vars` what a lookup or filter may read as the play's variables.
    ///
    /// # Errors
    /// [`BridgeError`], one per way a call ends without a value. Once the helper is gone every
    /// call fails the same way.
    pub fn call(
        &self,
        kind: PluginKind,
        name: &str,
        args: &[Value],
        kwargs: &Map<String, Value>,
        vars: Option<&Map<String, Value>>,
    ) -> Result<Value, BridgeError> {
        let mut guard = self.lock();
        // Once the helper is this call's: the wait for it is another call's time.
        let start = Instant::now();
        let answer = self.exchange(
            &mut guard,
            kind,
            name,
            // Built as a map, not with `json!`: that serializes the values again, and a float
            // that is not a number (`'nan' | float`) does not serialize, where the helper's
            // `json.loads` reads the `NaN` it is written as.
            Value::Object(Map::from_iter([
                ("kind".to_string(), json!(kind.as_str())),
                ("name".to_string(), json!(name)),
                ("args".to_string(), Value::Array(args.to_vec())),
                ("kwargs".to_string(), Value::Object(kwargs.clone())),
                (
                    "vars".to_string(),
                    vars.cloned().map_or(Value::Null, Value::Object),
                ),
            ])),
        );
        crate::profile::bridge_call(
            name,
            u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
        answer
    }

    /// The variables the lookup `name` reads, so that a call sends those and not the whole host.
    ///
    /// # Errors
    /// Why the helper could not say.
    pub fn lookup_needs(&self, name: &str) -> Result<LookupNeeds, BridgeError> {
        let cache = || {
            self.needs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        if let Some(known) = cache().get(name) {
            return Ok(known.clone());
        }
        let answer = self.request(
            PluginKind::Lookup,
            name,
            json!({"kind": "lookup_needs", "name": name, "args": [name], "kwargs": {}}),
        )?;
        let shape = || {
            BridgeError::Helper(format!(
                "the Python helper answered the variables of the lookup '{name}' in a shape it should not"
            ))
        };
        let names = answer
            .get("names")
            .and_then(Value::as_array)
            .ok_or_else(shape)?
            .iter()
            .map(|n| n.as_str().map(str::to_string).ok_or_else(shape))
            .collect::<Result<_, _>>()?;
        let all = answer
            .get("all")
            .and_then(Value::as_bool)
            .ok_or_else(shape)?;
        let needs = LookupNeeds { names, all };
        cache().insert(name.to_string(), needs.clone());
        Ok(needs)
    }

    /// Whether ansible-core has a filter or test of that name, without running it.
    ///
    /// # Errors
    /// [`BridgeError::NotFound`] when it has none, or why the helper could not say.
    pub fn resolve(&self, kind: PluginKind, name: &str) -> Result<(), BridgeError> {
        self.request(
            kind,
            name,
            json!({"kind": kind.as_str(), "name": name, "args": [], "kwargs": {}, "resolve_only": true}),
        )
        .map(drop)
    }

    /// Whether each name a `module_defaults` key spells resolves to a module or an action plugin
    /// ansible-core knows.
    ///
    /// # Errors
    /// [`BridgeError`] when the helper cannot start or answers out of shape.
    pub fn actions_resolve(
        &self,
        names: &[String],
    ) -> Result<std::collections::BTreeMap<String, bool>, BridgeError> {
        let answer = self.defaults_request("actions", names)?;
        names
            .iter()
            .map(|name| match answer.get(name) {
                Some(Value::Bool(found)) => Ok((name.clone(), *found)),
                _ => Err(defaults_shape()),
            })
            .collect()
    }

    /// The modules each `module_defaults` group (a full name) holds, as ansible-core resolves
    /// them; `None` for a group nobody declares.
    ///
    /// # Errors
    /// [`BridgeError`] when the helper cannot start or answers out of shape.
    pub fn action_groups(
        &self,
        groups: &[String],
    ) -> Result<std::collections::BTreeMap<String, Option<Vec<String>>>, BridgeError> {
        let answer = self.defaults_request("action_groups", groups)?;
        groups
            .iter()
            .map(|group| {
                let held = match answer.get(group).and_then(|g| g.get("actions")) {
                    None => None,
                    Some(Value::Array(items)) => Some(
                        items
                            .iter()
                            .map(|item| {
                                item.as_str().map(str::to_string).ok_or_else(defaults_shape)
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                    Some(_) => return Err(defaults_shape()),
                };
                Ok((group.clone(), held))
            })
            .collect()
    }

    /// The text ansible-playbook prints for a result's `diff`, from ansible-core's own
    /// `_get_diff`, with its lines coloured when `color` is set.
    ///
    /// # Errors
    /// [`BridgeError`] when the helper cannot start, fails, or answers something else than text.
    pub fn diff_text(&self, diff: &Value, color: bool) -> Result<String, BridgeError> {
        match self.request(
            PluginKind::Lookup,
            "diff",
            json!({"kind": "diff", "name": "diff", "args": [diff], "kwargs": {"color": color}}),
        )? {
            Value::String(text) => Ok(text),
            _ => Err(BridgeError::Helper(
                "the Python helper answered the diff in a shape it should not".into(),
            )),
        }
    }

    /// A collection inventory plugin run on `path` by ansible-core: the groups, hosts and
    /// variables it made, the warnings it said, or the `error` it ended with.
    ///
    /// # Errors
    /// [`BridgeError`] when the helper cannot start or breaks the protocol.
    pub fn inventory(&self, plugin: &str, path: &str) -> Result<Value, BridgeError> {
        self.request(
            PluginKind::Lookup,
            plugin,
            json!({"kind": "inventory", "name": plugin, "args": [path], "kwargs": {}}),
        )
    }

    /// [`Bridge::inventory`] for the plugin `plugin` itself rather than `auto`: `declined` when
    /// its `verify_file` refuses `path`.
    pub fn inventory_plugin(&self, plugin: &str, path: &str) -> Result<Value, BridgeError> {
        self.request(
            PluginKind::Lookup,
            plugin,
            json!({"kind": "inventory", "name": plugin, "args": [path], "kwargs": {"plugin": plugin}}),
        )
    }

    /// Where the controller's ansible-core finds the `kind` plugin `name` (`callback`, `cache`),
    /// `None` when it finds none or cannot be asked.
    pub fn plugin_path(&self, kind: &str, name: &str) -> Option<String> {
        self.request(
            PluginKind::Lookup,
            name,
            json!({"kind": "plugin_path", "name": name, "args": [kind], "kwargs": {}}),
        )
        .ok()?
        .as_str()
        .map(str::to_string)
    }

    /// Whether the controller's ansible-core loads an inventory plugin of that name.
    pub fn inventory_plugin_exists(&self, plugin: &str) -> bool {
        self.request(
            PluginKind::Lookup,
            plugin,
            json!({"kind": "inventory_plugin", "name": plugin, "args": [], "kwargs": {}}),
        )
        .is_ok_and(|found| found.as_bool() == Some(true))
    }

    fn defaults_request(
        &self,
        kind: &str,
        names: &[String],
    ) -> Result<Map<String, Value>, BridgeError> {
        match self.request(
            PluginKind::Lookup,
            kind,
            json!({"kind": kind, "name": kind, "args": names, "kwargs": {}}),
        )? {
            Value::Object(answer) => Ok(answer),
            _ => Err(defaults_shape()),
        }
    }

    fn request(&self, kind: PluginKind, name: &str, call: Value) -> Result<Value, BridgeError> {
        self.exchange(&mut self.lock(), kind, name, call)
    }

    /// One request and its answer, over the helper `guard` holds.
    fn exchange(
        &self,
        guard: &mut (State, u64),
        kind: PluginKind,
        name: &str,
        call: Value,
    ) -> Result<Value, BridgeError> {
        let (state, next) = guard;
        if matches!(state, State::Idle) {
            *state = self.started();
        }
        let helper = match state {
            State::Running(helper) => helper,
            State::Dead(why) => return Err(why.clone()),
            State::Idle => unreachable!("started above"),
        };
        *next += 1;
        let id = *next;
        // As in `call`: `json!` would serialize `call` again and refuse a `NaN` in it.
        let request = Value::Object(Map::from_iter([
            ("id".to_string(), json!(id)),
            ("call".to_string(), call),
        ]))
        .to_string();
        if request.len() > MAX_FRAME_LEN {
            return Err(BridgeError::Helper(format!(
                "the request for the {} plugin '{name}' is {} bytes, over the {MAX_FRAME_LEN} byte limit",
                kind.as_str(),
                request.len()
            )));
        }
        let outcome = answer(helper, id, request.as_bytes());
        if let Err(BridgeError::Helper(why)) = &outcome {
            // What the helper left unsaid is not read: the pipes are out of step from here on.
            let status = helper.end();
            let why = if why.starts_with("the Python helper exited") {
                format!("{why} ({status})")
            } else {
                why.clone()
            };
            let why = BridgeError::Helper(why);
            *state = State::Dead(why.clone());
            return Err(why);
        }
        outcome
    }
}

#[cfg(test)]
mod tests;
