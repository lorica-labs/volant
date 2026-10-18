// SPDX-License-Identifier: GPL-3.0-or-later
//! Filters, tests and lookups that only ansible-core can run, called in the controller's Python
//! helper.
//!
//! One helper serves the whole run, started on the first call. The helper renders nothing a play
//! holds: the arguments and variables arrive as data, already rendered here.
//!
//! Action plugins run in helpers of their own, one per action running at that moment. While a
//! plugin runs, its helper may ask the controller to act on the host, and waits for the reply.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use serde_json::{Map, Value, json};
use volant_protocol::frame::MAX_FRAME_LEN;

use crate::python::{ActionHelper, PythonBuilder};

mod conversation;
mod wire;

use conversation::{answer, converse, defaults_shape};
pub use wire::{Ask, BridgeError, PluginKind, Reply, TmpOp};

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
        self.request(
            kind,
            name,
            json!({"kind": kind.as_str(), "name": name, "args": args, "kwargs": kwargs, "vars": vars}),
        )
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
    /// `_get_diff`, without colour.
    ///
    /// # Errors
    /// [`BridgeError`] when the helper cannot start, fails, or answers something else than text.
    pub fn diff_text(&self, diff: &Value) -> Result<String, BridgeError> {
        match self.request(
            PluginKind::Lookup,
            "diff",
            json!({"kind": "diff", "name": "diff", "args": [diff], "kwargs": {}}),
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
        let mut guard = self.lock();
        let (state, next) = &mut *guard;
        if matches!(state, State::Idle) {
            *state = match (self.start)() {
                Ok(helper) => State::Running(helper),
                Err(err) => State::Dead(BridgeError::Start(format!("{err:#}"))),
            };
        }
        let helper = match state {
            State::Running(helper) => helper,
            State::Dead(why) => return Err(why.clone()),
            State::Idle => unreachable!("started above"),
        };
        *next += 1;
        let id = *next;
        let request = json!({"id": id, "call": call}).to_string();
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

    /// Runs the action plugin `name` for one task in a helper of the pool, as ansible-core runs
    /// it: `task` holds the task's keywords and `args`, `task_vars` the host's variables,
    /// `play_context` the fields of ansible-core's `PlayContext`. Each [`Ask`] the plugin makes
    /// is answered by `asks`, one at a time. The value is the plugin's result, a failed one when
    /// the plugin raised; it is data from the host and is never rendered again.
    ///
    /// # Errors
    /// [`BridgeError::NotFound`] when ansible-core has no such action plugin, `Plugin` when the
    /// request is refused, `Start` when no helper starts, and `Helper` when the helper dies,
    /// breaks the protocol or is still running at `deadline`: every
    /// wait for the helper ends there. A helper that ended any of those
    /// ways is killed, and the next action starts another.
    pub fn run_action(
        &self,
        name: &str,
        task: &Value,
        task_vars: &Value,
        play_context: &Value,
        deadline: Instant,
        asks: &mut dyn FnMut(Ask) -> Reply,
    ) -> Result<Value, BridgeError> {
        let (mut helper, id) = self.action_helper()?;
        let request = json!({"id": id, "action": {
            "name": name, "task": task, "task_vars": task_vars, "play_context": play_context,
        }})
        .to_string();
        if request.len() > MAX_FRAME_LEN {
            self.lock_actions().idle.push(helper);
            return Err(BridgeError::Helper(format!(
                "the request for the action plugin '{name}' is {} bytes, over the {MAX_FRAME_LEN} byte limit",
                request.len()
            )));
        }
        match converse(&mut helper, id, name, &request, deadline, asks) {
            Err(BridgeError::Helper(why)) => {
                // What the helper left unsaid is not read: it is ended, and the pool loses it.
                let status = helper.end();
                Err(BridgeError::Helper(
                    if why.starts_with("the Python helper exited") {
                        format!("{why} ({status})")
                    } else {
                        why
                    },
                ))
            }
            outcome => {
                self.lock_actions().idle.push(helper);
                outcome
            }
        }
    }

    fn lock_actions(&self) -> MutexGuard<'_, Actions> {
        self.actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// An idle helper of the pool, or a new one, and the id of the request it is about to get.
    fn action_helper(&self) -> Result<(ActionHelper, u64), BridgeError> {
        let mut actions = self.lock_actions();
        if let Some(why) = &actions.broken {
            return Err(why.clone());
        }
        actions.next += 1;
        let id = actions.next;
        // A helper that died while idle is dropped here, which reaps it, and never reused.
        while let Some(mut helper) = actions.idle.pop() {
            if helper.alive() {
                return Ok((helper, id));
            }
        }
        // Started without the lock: an action on another host need not wait for this start.
        drop(actions);
        match (self.start)() {
            Ok(helper) => Ok((ActionHelper::new(helper), id)),
            Err(err) => {
                let why = BridgeError::Start(format!("{err:#}"));
                self.lock_actions().broken = Some(why.clone());
                Err(why)
            }
        }
    }
}

#[cfg(test)]
mod tests;
