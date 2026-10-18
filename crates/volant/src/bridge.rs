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

use crate::python::{ActionHelper, PythonBuilder, Silence};

/// What an action plugin asks of the host while it runs, done by the controller over the host's
/// own link.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    /// Run a module the controller's way. `files` pairs an argument with the controller file it
    /// stages.
    Module {
        name: String,
        args: Value,
        files: Vec<(String, PathBuf)>,
    },
    /// `Connection.exec_command`: a command line, already wrapped in its shell.
    Command {
        cmd: String,
        stdin: Option<Vec<u8>>,
        sudoable: bool,
    },
    /// `Connection.put_file`: a controller file written at a path of the host.
    PutFile {
        local: PathBuf,
        remote: String,
        mode: Option<u32>,
    },
    /// `Connection.fetch_file`: a file of the host written at a controller path.
    FetchFile { remote: String, local: PathBuf },
    /// The action's temporary directory on the host.
    Tmp(TmpOp),
    /// The variables of another host, which `task_vars` leaves out until a plugin reads them.
    HostVar { host: String },
}

/// What happens to an action's temporary directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmpOp {
    /// Make one, as `remote_user` when the plugin names one; the reply is its path.
    Make {
        remote_user: Option<String>,
    },
    Remove {
        path: String,
    },
}

/// The controller's answer to an [`Ask`]. For a `Command` the value is `{rc, stdout, stderr}`,
/// for a `Module` its result, for `Tmp(Make)` the path, for a `HostVar` the variables or `null`
/// when there is no such host.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Value(Value),
    Error(String),
}

impl Ask {
    fn read(ask: &Value) -> Result<Ask, String> {
        let text = |key: &str| {
            ask.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("{key} is not text"))
        };
        let maybe_text = |key: &str| match ask.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(format!("{key} is not text")),
        };
        Ok(match ask.get("op").and_then(Value::as_str) {
            Some("module") => Ask::Module {
                name: text("name")?,
                args: ask
                    .get("args")
                    .filter(|args| args.is_object())
                    .cloned()
                    .ok_or("the module arguments are not a mapping")?,
                files: match ask.get("files") {
                    None => Vec::new(),
                    Some(Value::Array(files)) => files
                        .iter()
                        .map(|pair| match pair.as_array().map(Vec::as_slice) {
                            Some([Value::String(arg), Value::String(path)]) => {
                                Ok((arg.clone(), PathBuf::from(path)))
                            }
                            _ => Err("a staged file is not an argument and a path".to_string()),
                        })
                        .collect::<Result<_, _>>()?,
                    Some(_) => return Err("the staged files are not a list".into()),
                },
            },
            Some("command") => Ask::Command {
                cmd: text("cmd")?,
                stdin: maybe_text("in_data")?.map(String::into_bytes),
                sudoable: ask
                    .get("sudoable")
                    .and_then(Value::as_bool)
                    .ok_or("sudoable is not a boolean")?,
            },
            Some("put_file") => Ask::PutFile {
                local: text("local")?.into(),
                remote: text("remote")?,
                mode: match ask.get("mode") {
                    None | Some(Value::Null) => None,
                    Some(mode) => Some(
                        mode.as_u64()
                            .and_then(|mode| u32::try_from(mode).ok())
                            .filter(|mode| *mode <= 0o7777)
                            .ok_or("the mode is not a file mode")?,
                    ),
                },
            },
            Some("fetch_file") => Ask::FetchFile {
                remote: text("remote")?,
                local: text("local")?.into(),
            },
            Some("tmp") => Ask::Tmp(match ask.get("action").and_then(Value::as_str) {
                Some("make") => TmpOp::Make {
                    remote_user: maybe_text("remote_user")?,
                },
                Some("remove") => TmpOp::Remove {
                    path: text("path")?,
                },
                other => return Err(format!("the temporary directory action {other:?}")),
            }),
            Some("hostvar") => Ask::HostVar {
                host: text("host")?,
            },
            other => return Err(format!("the operation {other:?}")),
        })
    }
}

impl Reply {
    fn to_json(&self) -> Value {
        match self {
            Reply::Value(value) => json!({ "value": value }),
            Reply::Error(text) => json!({ "error": text }),
        }
    }
}

/// What a bridged call runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginKind {
    Filter,
    Test,
    Lookup,
}

impl PluginKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PluginKind::Filter => "filter",
            PluginKind::Test => "test",
            PluginKind::Lookup => "lookup",
        }
    }
}

/// Why a bridged call gave no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// The plugin ran and failed; the text is ansible-core's, `The filter plugin 'x' failed: …`.
    Plugin(String),
    /// ansible-core knows no plugin of that name; the text is its sentence.
    NotFound(String),
    /// The helper could not start; the text is why.
    Start(String),
    /// The helper died or broke the protocol.
    Helper(String),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BridgeError::Start(why) => write!(f, "the Python helper could not start: {why}"),
            BridgeError::Plugin(text) | BridgeError::NotFound(text) | BridgeError::Helper(text) => {
                f.write_str(text)
            }
        }
    }
}

impl std::error::Error for BridgeError {}

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

/// One action request, the asks it makes answered in turn, until its value or its error.
fn converse(
    helper: &mut ActionHelper,
    id: u64,
    name: &str,
    request: &str,
    deadline: Instant,
    asks: &mut dyn FnMut(Ask) -> Reply,
) -> Result<Value, BridgeError> {
    helper
        .send(request.as_bytes())
        .map_err(|err| exited(&err))?;
    loop {
        let frame = match helper.next(deadline) {
            Ok(Ok(Some(frame))) => frame,
            Ok(Ok(None)) | Err(Silence::Gone) => {
                return Err(exited(&"it closed its output before answering"));
            }
            Ok(Err(err)) => return Err(unreadable(&err)),
            Err(Silence::Expired) => {
                return Err(BridgeError::Helper(format!(
                    "the action plugin '{name}' was still running at the task's deadline"
                )));
            }
        };
        let message = decode(&frame, id)?;
        let Some(question) = message.get("ask") else {
            return outcome(message);
        };
        let ask = Ask::read(question).map_err(|why| {
            BridgeError::Helper(format!(
                "the Python helper asked something out of shape: {why}"
            ))
        })?;
        let reply = json!({"id": id, "reply": asks(ask).to_json()}).to_string();
        // An answer the helper could not read becomes an error the plugin can.
        let reply = if reply.len() > MAX_FRAME_LEN {
            json!({"id": id, "reply": {"error": format!(
                "the answer is {} bytes, over the {MAX_FRAME_LEN} byte limit",
                reply.len()
            )}})
            .to_string()
        } else {
            reply
        };
        helper.send(reply.as_bytes()).map_err(|err| exited(&err))?;
    }
}

fn exited(detail: &dyn fmt::Display) -> BridgeError {
    BridgeError::Helper(format!("the Python helper exited: {detail}"))
}

/// A read that failed: a frame over the cap is the helper breaking the protocol.
fn unreadable(err: &std::io::Error) -> BridgeError {
    if err.kind() == std::io::ErrorKind::InvalidData {
        BridgeError::Helper(format!("the Python helper broke the protocol: {err}"))
    } else {
        exited(err)
    }
}

fn defaults_shape() -> BridgeError {
    BridgeError::Helper(
        "the Python helper answered the module_defaults names in a shape it should not".into(),
    )
}

/// One request and its answer, checked to be the answer to that request.
fn answer(helper: &mut PythonBuilder, id: u64, request: &[u8]) -> Result<Value, BridgeError> {
    let frame = match helper.round_trip(request) {
        Ok(Some(frame)) => frame,
        Ok(None) => return Err(exited(&"it closed its output before answering")),
        Err(err) => return Err(unreadable(&err)),
    };
    outcome(decode(&frame, id)?)
}

/// A frame read as the JSON message of request `id`.
fn decode(frame: &[u8], id: u64) -> Result<Value, BridgeError> {
    let text = std::str::from_utf8(frame).map_err(|err| {
        BridgeError::Helper(format!(
            "the Python helper answered bytes that are not UTF-8: {err}"
        ))
    })?;
    let reply: Value = serde_json::from_str(text).map_err(|err| {
        BridgeError::Helper(format!(
            "the Python helper answered something that is not JSON: {err}"
        ))
    })?;
    if reply.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(BridgeError::Helper(format!(
            "the Python helper answered request {} to request {id}",
            reply
                .get("id")
                .map_or("none".to_string(), ToString::to_string)
        )));
    }
    Ok(reply)
}

/// The value a final message carries, or its error.
fn outcome(mut reply: Value) -> Result<Value, BridgeError> {
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no message")
            .to_string();
        return Err(match error.get("kind").and_then(Value::as_str) {
            Some("plugin") => BridgeError::Plugin(message),
            Some("not_found") => BridgeError::NotFound(message),
            other => BridgeError::Helper(format!(
                "the Python helper reported an error of kind {other:?}: {message}"
            )),
        });
    }
    match reply.get_mut("value") {
        Some(value) => Ok(value.take()),
        None => Err(BridgeError::Helper(
            "the Python helper's answer carries neither a value nor an error".into(),
        )),
    }
}

#[cfg(test)]
mod tests;
