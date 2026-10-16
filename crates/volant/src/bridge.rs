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
// ponytail: one helper per action running at once, so as many as the run's forks; cap the pool
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
}

impl Bridge {
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
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rstest::rstest;

    use super::*;

    /// A stand-in helper: it reads each request and acts as `argv[1]` says.
    const STAND_IN: &str = r#"
import json, os, struct, sys, time
mode = sys.argv[1]
out = sys.stdout.buffer
def put(raw):
    out.write(struct.pack(">I", len(raw)) + raw)
    out.flush()
def get():
    head = sys.stdin.buffer.read(4)
    if len(head) < 4:
        sys.exit(0)
    return json.loads(sys.stdin.buffer.read(struct.unpack(">I", head)[0]))
n = 0
while True:
    request = get()
    n += 1
    if "action" in request and mode in ("asks", "ask_wrong_id", "die_mid_ask", "silent", "bad_ask"):
        if mode == "silent":
            time.sleep(600)
        ident = request["id"] + (1 if mode == "ask_wrong_id" else 0)
        questions = [{"op": "command", "cmd": "true", "in_data": "in", "sudoable": True}, {"op": "hostvar", "host": "h2"}]
        if mode == "bad_ask":
            questions = [{"op": "put_file", "local": "/a", "remote": "/b", "mode": 99999}]
        replies = []
        for question in questions:
            put(json.dumps({"id": ident, "ask": question}).encode())
            if mode == "die_mid_ask":
                os._exit(3)
            replies.append(get())
        put(json.dumps({"id": ident, "value": replies}).encode())
        continue
    if mode == "die_second" and n == 2:
        sys.stderr.write("gone\n")
        os._exit(3)
    if mode == "wrong_id":
        put(json.dumps({"id": request["id"] + 1, "value": 1}).encode())
    elif mode == "oversized":
        out.write(struct.pack(">I", 64 * 1024 * 1024 + 1)); out.flush()
    elif mode == "defaults":
        call = request["call"]
        names = call["args"]
        if call["kind"] == "actions":
            value = {n: n != "ansible.legacy.nosuch" for n in names}
        else:
            value = {n: {"missing": True} if n.endswith("nosuch") else {"actions": ["ansible.builtin.ping"]} for n in names}
        put(json.dumps({"id": request["id"], "value": value}).encode())
    elif mode == "bad_items":
        put(json.dumps({"id": request["id"], "value": {n: {"actions": [1]} for n in request["call"]["args"]}}).encode())
    elif mode == "latin1":
        put(b"\xff\xfe")
    else:
        put(json.dumps({"id": request["id"], "value": request["call"]["args"]}).encode())
"#;

    fn bridge_with(mode: &'static str, starts: &Arc<AtomicUsize>) -> Arc<Bridge> {
        let starts = Arc::clone(starts);
        Bridge::with_start(Box::new(move || {
            starts.fetch_add(1, Ordering::SeqCst);
            PythonBuilder::spawn("python3", &["-c", STAND_IN, mode], &[])
        }))
    }

    fn ask(bridge: &Bridge) -> Result<Value, BridgeError> {
        bridge.call(
            PluginKind::Filter,
            "upper",
            &[json!("a")],
            &Map::new(),
            None,
        )
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_module_defaults_requests_read_the_helper_s_answers() {
        let bridge = bridge_with("defaults", &Arc::new(AtomicUsize::new(0)));
        let found = bridge
            .actions_resolve(&names(&["ansible.legacy.foo", "ansible.legacy.nosuch"]))
            .expect("an answer");
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            [
                ("ansible.legacy.foo".to_string(), true),
                ("ansible.legacy.nosuch".to_string(), false)
            ]
        );
        let held = bridge
            .action_groups(&names(&["ansible.builtin.g", "my.col.nosuch"]))
            .expect("an answer");
        assert_eq!(
            held["ansible.builtin.g"],
            Some(names(&["ansible.builtin.ping"]))
        );
        assert_eq!(held["my.col.nosuch"], None);
    }

    #[rstest]
    #[case::echo("echo")]
    #[case::items_that_are_not_names("bad_items")]
    fn module_defaults_answers_out_of_shape_are_refused(#[case] mode: &'static str) {
        let bridge = bridge_with(mode, &Arc::new(AtomicUsize::new(0)));
        let groups = bridge
            .action_groups(&names(&["ansible.builtin.g"]))
            .unwrap_err();
        assert!(
            groups.to_string().contains("in a shape it should not"),
            "{groups}"
        );
        let actions = bridge.actions_resolve(&names(&["x"])).unwrap_err();
        assert!(
            actions.to_string().contains("in a shape it should not"),
            "{actions}"
        );
    }

    #[test]
    fn module_defaults_are_asked_about_once_and_refused_by_name() {
        use crate::collection::Resolved;
        use crate::playbook::PlayTask;

        let task = |key: &str| {
            let mut task = PlayTask::empty();
            task.module_defaults = vec![json!({ key: {"a": 1} })];
            task
        };
        let bridge = bridge_with("defaults", &Arc::new(AtomicUsize::new(0)));
        let resolved = Resolved::default();
        let group = task("group/g");
        resolved.ensure([&group], Some(&bridge)).expect("a group");
        assert_eq!(
            resolved.groups()["ansible.builtin.g"],
            names(&["ansible.builtin.ping"])
        );
        assert_eq!(
            resolved.ensure([&task("group/nosuch")], Some(&bridge)),
            Err("Error loading module_defaults: could not resolve the module_defaults group ansible.builtin.nosuch".to_string())
        );
        assert_eq!(
            resolved.ensure([&task("nosuch")], Some(&bridge)),
            Err("Could not resolve action ansible.legacy.nosuch in module_defaults".to_string())
        );
        assert_eq!(resolved.ensure([&task("foo")], Some(&bridge)), Ok(()));
        // Without a helper nothing can be checked, and the run says which name.
        assert_eq!(
            Resolved::default().ensure([&group], None),
            Err("Error loading module_defaults: could not resolve the module_defaults group ansible.builtin.g".to_string())
        );
    }

    #[test]
    fn a_healthy_helper_answers_and_starts_once() {
        let starts = Arc::new(AtomicUsize::new(0));
        let bridge = bridge_with("echo", &starts);
        assert_eq!(ask(&bridge), Ok(json!(["a"])));
        assert_eq!(ask(&bridge), Ok(json!(["a"])));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[rstest]
    #[case::wrong_id("wrong_id", "answered request 2 to request 1")]
    #[case::oversized_frame("oversized", "exceeds the 67108864 byte limit")]
    #[case::not_utf8("latin1", "not UTF-8")]
    fn a_helper_that_breaks_the_protocol_ends_the_bridge(
        #[case] mode: &'static str,
        #[case] said: &str,
    ) {
        let starts = Arc::new(AtomicUsize::new(0));
        let bridge = bridge_with(mode, &starts);
        let first = ask(&bridge).unwrap_err();
        assert!(first.to_string().contains(said), "{first}");
        assert_eq!(ask(&bridge), Err(first));
        assert_eq!(starts.load(Ordering::SeqCst), 1, "the helper was restarted");
    }

    #[test]
    fn a_helper_killed_between_two_calls_fails_every_later_call() {
        let starts = Arc::new(AtomicUsize::new(0));
        let bridge = bridge_with("die_second", &starts);
        assert!(ask(&bridge).is_ok());
        let second = ask(&bridge).unwrap_err();
        assert!(
            second.to_string().starts_with("the Python helper exited"),
            "{second}"
        );
        assert_eq!(ask(&bridge), Err(second));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_helper_that_cannot_start_names_why_on_every_call() {
        let bridge = Bridge::with_start(Box::new(|| anyhow::bail!("no ansible-core")));
        let err = ask(&bridge).unwrap_err();
        assert_eq!(err, BridgeError::Start("no ansible-core".into()));
        assert_eq!(
            err.to_string(),
            "the Python helper could not start: no ansible-core"
        );
        assert_eq!(ask(&bridge), Err(err));
    }

    #[test]
    fn a_request_over_the_cap_is_refused_without_ending_the_bridge() {
        let starts = Arc::new(AtomicUsize::new(0));
        let bridge = bridge_with("echo", &starts);
        let huge = json!("x".repeat(MAX_FRAME_LEN));
        let err = bridge
            .call(PluginKind::Filter, "upper", &[huge], &Map::new(), None)
            .unwrap_err();
        assert!(err.to_string().contains("byte limit"), "{err}");
        assert_eq!(ask(&bridge), Ok(json!(["a"])));
    }

    /// Runs one action on `bridge` on a thread of its own, answering every ask with `reply`, and
    /// gives back what was asked and how the action ended. A run that hangs fails the test.
    fn act(
        bridge: &Arc<Bridge>,
        deadline: Instant,
        reply: fn(&Ask) -> Reply,
    ) -> (Vec<Ask>, Result<Value, BridgeError>) {
        let bridge = Arc::clone(bridge);
        let (send, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut asked = Vec::new();
            let ended = bridge.run_action(
                "my.col.act",
                &json!({"args": {}}),
                &json!({}),
                &json!({}),
                deadline,
                &mut |ask| {
                    let answer = reply(&ask);
                    asked.push(ask);
                    answer
                },
            );
            let _ = send.send((asked, ended));
        });
        done.recv_timeout(std::time::Duration::from_secs(60))
            .expect("the action ended")
    }

    /// A deadline no test row is meant to reach.
    fn later() -> Instant {
        Instant::now() + std::time::Duration::from_secs(60)
    }

    fn answers(ask: &Ask) -> Reply {
        match ask {
            Ask::HostVar { .. } => Reply::Error("no such host".into()),
            _ => Reply::Value(json!("ok")),
        }
    }

    #[test]
    fn an_action_s_asks_are_answered_in_turn_and_its_helper_kept() {
        let starts = Arc::new(AtomicUsize::new(0));
        let bridge = bridge_with("asks", &starts);
        for _ in 0..2 {
            let (asked, ended) = act(&bridge, later(), answers);
            assert_eq!(
                asked,
                [
                    Ask::Command {
                        cmd: "true".into(),
                        stdin: Some(b"in".to_vec()),
                        sudoable: true
                    },
                    Ask::HostVar { host: "h2".into() }
                ]
            );
            let id = ended.as_ref().expect("a value")[0]["id"].clone();
            assert_eq!(
                ended,
                Ok(json!([
                    {"id": id, "reply": {"value": "ok"}},
                    {"id": id, "reply": {"error": "no such host"}}
                ]))
            );
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_answer_over_the_cap_reaches_the_plugin_as_an_error() {
        let bridge = bridge_with("asks", &Arc::new(AtomicUsize::new(0)));
        let (_, ended) = act(&bridge, later(), |_| {
            Reply::Value(json!("x".repeat(MAX_FRAME_LEN)))
        });
        let ended = ended.expect("a value");
        let error = ended[0]["reply"]["error"].as_str().unwrap_or_default();
        assert!(error.contains("over the 67108864 byte limit"), "{ended}");
    }

    #[rstest]
    #[case::ask_for_another_request("ask_wrong_id", "answered request")]
    #[case::death_mid_ask("die_mid_ask", "the Python helper exited")]
    #[case::oversized_frame("oversized", "exceeds the 67108864 byte limit")]
    #[case::ask_out_of_shape("bad_ask", "the mode is not a file mode")]
    #[case::still_running_at_the_deadline("silent", "was still running at the task's deadline")]
    fn an_action_whose_helper_fails_ends_it_and_the_next_starts_another(
        #[case] mode: &'static str,
        #[case] said: &str,
    ) {
        let starts = Arc::new(AtomicUsize::new(0));
        let bridge = bridge_with(mode, &starts);
        // Only the silent helper is meant to reach its deadline.
        let wait = if mode == "silent" { 500 } else { 60_000 };
        let deadline = Instant::now() + std::time::Duration::from_millis(wait);
        for start in 1..=2 {
            let (_, ended) = act(&bridge, deadline, answers);
            let err = ended.unwrap_err();
            assert!(err.to_string().contains(said), "{err}");
            assert_eq!(starts.load(Ordering::SeqCst), start);
        }
    }

    #[test]
    fn actions_that_cannot_start_a_helper_name_why_without_retrying() {
        let starts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&starts);
        let bridge = Bridge::with_start(Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("no ansible-core")
        }));
        for _ in 0..2 {
            let (_, ended) = act(&bridge, later(), answers);
            assert_eq!(ended, Err(BridgeError::Start("no ansible-core".into())));
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }
}
