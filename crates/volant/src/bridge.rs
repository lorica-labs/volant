// SPDX-License-Identifier: GPL-3.0-or-later
//! Filters, tests and lookups that only ansible-core can run, called in the controller's Python
//! helper.
//!
//! One helper serves the whole run, started on the first call. The helper renders nothing a play
//! holds: the arguments and variables arrive as data, already rendered here.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Map, Value, json};
use volant_protocol::frame::MAX_FRAME_LEN;

use crate::python::PythonBuilder;

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

/// The run's channel to the Python helper.
pub struct Bridge {
    start: Start,
    state: Mutex<(State, u64)>,
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
}

fn defaults_shape() -> BridgeError {
    BridgeError::Helper(
        "the Python helper answered the module_defaults names in a shape it should not".into(),
    )
}

/// One request and its answer, checked to be the answer to that request.
fn answer(helper: &mut PythonBuilder, id: u64, request: &[u8]) -> Result<Value, BridgeError> {
    let gone = |detail: &dyn fmt::Display| {
        BridgeError::Helper(format!("the Python helper exited: {detail}"))
    };
    let frame = match helper.round_trip(request) {
        Ok(Some(frame)) => frame,
        Ok(None) => return Err(gone(&"it closed its output before answering")),
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
            return Err(BridgeError::Helper(format!(
                "the Python helper broke the protocol: {err}"
            )));
        }
        Err(err) => return Err(gone(&err)),
    };
    let text = std::str::from_utf8(&frame).map_err(|err| {
        BridgeError::Helper(format!(
            "the Python helper answered bytes that are not UTF-8: {err}"
        ))
    })?;
    let mut reply: Value = serde_json::from_str(text).map_err(|err| {
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
import json, os, struct, sys
mode = sys.argv[1]
out = sys.stdout.buffer
def put(raw):
    out.write(struct.pack(">I", len(raw)) + raw)
    out.flush()
n = 0
while True:
    head = sys.stdin.buffer.read(4)
    if len(head) < 4:
        break
    request = json.loads(sys.stdin.buffer.read(struct.unpack(">I", head)[0]))
    n += 1
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
}
