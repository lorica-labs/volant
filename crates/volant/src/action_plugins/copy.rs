// SPDX-License-Identifier: GPL-3.0-or-later
//! `copy`: a controller file, or a `content:`, sent to the host only when it differs.
//!
//! Read off `plugins/action/copy.py` of ansible-core 2.19.12 and measured against it. The plugin
//! runs `stat` on the destination, compares the SHA-1 it reports with the one of the local
//! bytes, and then either runs `file` to set the attributes of a destination that is already
//! right, or stages the bytes on the host and runs `copy` on them. Only that last branch sends
//! anything: a destination that is already right receives nothing.
//!
//! One refusal is this engine's own, and stricter than the reference: a `src` whose render read
//! a managed host is never looked up. Measured, the reference copies whatever controller file a
//! registered value names, so a host that controls a command's output can have any file the
//! operator can read sent to it.

use std::fmt::Write as _;

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::encoding::sha1_hex;

use super::files::{blob_of, not_found, search_paths};
use super::{Context, Plugin, Step, Sub, lost};
use crate::executor::as_bool_value;

/// What the reference answers for a `src` a managed host chose, which it does send.
const HOST_NAMED: &str = "the 'src' of this task was named by a managed host, and a controller file a host chose is never sent";

/// The arguments the reference hands on to `file` when the destination is already right: its
/// `REAL_FILE_ARGS`, less `src`, which it deletes.
const FILE_ARGS: &[&str] = &[
    "mode",
    "owner",
    "group",
    "seuser",
    "serole",
    "selevel",
    "setype",
    "attributes",
    "unsafe_writes",
    "state",
    "path",
    "_original_basename",
    "recurse",
    "force",
    "_diff_peek",
];

/// The copy action from the point where its source bytes are in hand: `template` enters here
/// with the rendered text, `copy` with the file it found or the `content` it was given.
pub(crate) struct CopyOf {
    pub bytes: Vec<u8>,
    /// The source's file name, which the host appends to a destination that is a directory.
    pub basename: String,
    /// The task's arguments. A `content` among them is what makes a directory destination a
    /// refusal, and `src`, when there is one, is what a `force: false` result reports.
    pub args: Map<String, Value>,
}

/// A boolean argument, read the way the reference reads one, or its default.
fn flag(args: &Map<String, Value>, name: &str, default: bool) -> bool {
    args.get(name).and_then(as_bool_value).unwrap_or(default)
}

fn text<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

pub(super) fn start(ctx: Context<'_>) -> Box<dyn Plugin> {
    match source(&ctx) {
        Ok(Some(of)) => copy_bytes(of),
        // Measured: with `remote_src` the module runs alone, with the task's arguments as they
        // are, and no `stat` before it.
        Ok(None) => Box::new(Then(Some(Sub::run("copy", ctx.args.clone())))),
        Err(msg) => Box::new(Then(Some(Step::Done(TaskResult::failed_with(msg))))),
    }
}

/// The source bytes, `None` for a `remote_src` the host copies by itself, or the refusal.
fn source(ctx: &Context<'_>) -> Result<Option<CopyOf>, String> {
    let args = ctx.args;
    let content = args.get("content").filter(|c| !c.is_null());
    let dest = text(args, "dest");
    // The reference's checks, in its order and in its words.
    let src = match (text(args, "src"), content) {
        (None, None) => return Err("src (or content) is required".into()),
        _ if dest.is_none() => return Err("dest is required".into()),
        (Some(_), Some(_)) => return Err("src and content are mutually exclusive".into()),
        (None, Some(content)) => {
            if dest.is_some_and(|d| d.ends_with('/')) {
                return Err("can not use content with a dir as dest".into());
            }
            let bytes = content_text(content).into_bytes();
            // The reference names the temporary file it writes the content to, `.1f6t2baj` in
            // the measurement. The name means nothing beyond itself: nothing is ever written
            // under it, since a directory destination is refused for a `content`.
            let basename = format!(".{}", &sha1_hex(&bytes)[..8]);
            return Ok(Some(CopyOf {
                bytes,
                basename,
                args: args.clone(),
            }));
        }
        (Some(src), None) => src,
    };
    if flag(args, "remote_src", false) {
        return Ok(None);
    }
    // Before the name is so much as looked up: a lookup that answers "not found" already tells
    // a host whether a controller path exists.
    if ctx.args_untrusted.contains("src") {
        return Err(HOST_NAMED.into());
    }
    let searched = search_paths(ctx.origin, ctx.playbook_dir, "files", src);
    let Some(found) = searched.iter().find(|p| p.exists()) else {
        return Err(format!(
            "Unexpected AnsibleActionFail error: {}",
            not_found(src, &searched)
        ));
    };
    if found.is_dir() {
        return Err(format!("copying a directory is not supported yet: {src}"));
    }
    let bytes = std::fs::read(found)
        .map_err(|err| format!("could not read src={}: {err}", found.display()))?;
    let mut args = args.clone();
    args.insert("src".into(), Value::String(found.display().to_string()));
    // Read off the local file, as the reference does. Left to the module, `preserve` would
    // read the mode of the private copy the agent staged, and set that.
    if args.get("mode").and_then(Value::as_str) == Some("preserve") {
        args.insert("mode".into(), Value::String(local_mode(found)?));
    }
    Ok(Some(CopyOf {
        bytes,
        basename: found
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        args,
    }))
}

#[cfg(unix)]
fn local_mode(path: &std::path::Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|err| format!("could not read the mode of {}: {err}", path.display()))?
        .permissions()
        .mode();
    Ok(format!("0{:03o}", mode & 0o7777))
}

#[cfg(not(unix))]
fn local_mode(path: &std::path::Path) -> Result<String, String> {
    Err(format!(
        "mode: preserve reads a unix mode, and {} has none on this controller",
        path.display()
    ))
}

/// A `content` as the reference writes it to its temporary file: a string as it is, a mapping
/// or a list through `json.dumps`, anything else as Python prints it.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => python_dumps(other),
    }
}

/// `json.dumps` with its defaults: `", "` and `": "` between items, keys in their order, and
/// every character past ASCII escaped.
fn python_dumps(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let fields: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", ascii_string(k), python_dumps(v)))
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(python_dumps).collect();
            format!("[{}]", items.join(", "))
        }
        Value::String(s) => ascii_string(s),
        other => other.to_string(),
    }
}

fn ascii_string(s: &str) -> String {
    let mut out = String::new();
    for c in Value::String(s.to_string()).to_string().chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out
}

/// A plugin with one step to hand, then the result of that step as the task's.
struct Then(Option<Step>);

impl Plugin for Then {
    fn next(&mut self, last: Option<TaskResult>) -> Step {
        self.0
            .take()
            .unwrap_or_else(|| Step::Done(last.unwrap_or_else(|| lost("copy"))))
    }
}

enum State {
    Start,
    /// `again` once the destination turned out to be a directory and the name inside it is
    /// being looked at.
    Stat {
        again: bool,
    },
    Module(&'static str),
}

struct Copy {
    of: CopyOf,
    /// SHA-1 of the bytes, the sum the host's `stat` reports.
    checksum: String,
    /// The task's `dest`, which the module is handed whatever the plugin looked at.
    dest: String,
    /// The path the plugin looks at: `dest`, or the source's name inside it.
    dest_file: String,
    force: bool,
    follow: bool,
    state: State,
}

pub(crate) fn copy_bytes(of: CopyOf) -> Box<dyn Plugin> {
    let dest = text(&of.args, "dest").unwrap_or_default().to_string();
    let dest_file = if dest.ends_with('/') {
        inside(&dest, &of.basename)
    } else {
        dest.clone()
    };
    Box::new(Copy {
        checksum: sha1_hex(&of.bytes),
        force: flag(&of.args, "force", true),
        follow: flag(&of.args, "follow", false),
        dest,
        dest_file,
        of,
        state: State::Start,
    })
}

fn inside(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The `stat` the reference runs, measured: no size, and a SHA-1 unless `force` is off, in
/// which case a destination that exists is left alone whatever it holds.
fn stat(path: &str, follow: bool, checksum: bool) -> Step {
    let mut args = Map::new();
    args.insert("path".into(), Value::String(path.to_string()));
    args.insert("follow".into(), Value::Bool(follow));
    args.insert("get_checksum".into(), Value::Bool(checksum));
    args.insert("checksum_algorithm".into(), Value::String("sha1".into()));
    args.insert("get_size".into(), Value::Bool(false));
    Sub::run("stat", args)
}

/// The `stat` block of a `stat` result, or the reference's failure when there is none.
fn stat_of(path: &str, result: TaskResult) -> Result<Map<String, Value>, TaskResult> {
    if !result.failed()
        && let Some(Value::Object(stat)) = result.0.get("stat")
    {
        return Ok(stat.clone());
    }
    let msg = ["module_stderr", "module_stdout", "msg"]
        .iter()
        .find_map(|key| {
            result
                .0
                .get(*key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("None");
    Err(TaskResult::failed_with(format!(
        "Failed to get information on remote file ({path}): {msg}"
    )))
}

impl Copy {
    fn looked_at(&mut self, found: &Map<String, Value>, again: bool) -> Step {
        let is = |key: &str| found.get(key).and_then(Value::as_bool).unwrap_or(false);
        let exists = is("exists");
        if exists && is("isdir") && !again {
            if self.of.args.get("content").is_some_and(|c| !c.is_null()) {
                return Step::Done(TaskResult::failed_with(
                    "can not use content with a dir as dest",
                ));
            }
            self.dest_file = inside(&self.dest, &self.of.basename);
            self.state = State::Stat { again: true };
            return stat(&self.dest_file, self.follow, self.force);
        }
        if exists && !self.force {
            // Measured: the destination is left alone, and the result says which one and from
            // what, nothing more.
            let mut result = Map::new();
            result.insert("changed".into(), Value::Bool(false));
            result.insert("dest".into(), Value::String(self.dest.clone()));
            result.insert(
                "src".into(),
                self.of
                    .args
                    .get("src")
                    .cloned()
                    .unwrap_or_else(|| Value::String(self.of.basename.clone())),
            );
            return Step::Done(TaskResult(result));
        }
        // What the reference compares with when there is no file: a sum no file has.
        let remote = if exists {
            found.get("checksum").and_then(Value::as_str).unwrap_or("")
        } else {
            "1"
        };
        if remote == self.checksum {
            self.touch()
        } else {
            self.send()
        }
    }

    /// The destination already holds these bytes: `file` sets what else the task asked for.
    fn touch(&mut self) -> Step {
        let mut args: Map<String, Value> = self
            .of
            .args
            .iter()
            .filter(|(k, _)| FILE_ARGS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        args.insert("dest".into(), Value::String(self.dest.clone()));
        args.insert(
            "_original_basename".into(),
            Value::String(self.of.basename.clone()),
        );
        args.insert("recurse".into(), Value::Bool(false));
        args.insert("state".into(), Value::String("file".into()));
        self.state = State::Module("file");
        Sub::run("file", args)
    }

    /// The destination differs: the bytes are staged on the host as `src`, and `copy` moves
    /// them into place.
    fn send(&mut self) -> Step {
        let blob = match blob_of(&self.of.basename, &self.of.bytes) {
            Ok(blob) => blob,
            Err(msg) => return Step::Done(TaskResult::failed_with(msg)),
        };
        let mut args = self.of.args.clone();
        // `src` is the agent's to write, and only the staged path may stand there: a path of
        // the controller's left in it would be read on the host.
        for key in ["content", "decrypt", "src"] {
            args.remove(key);
        }
        args.insert("dest".into(), Value::String(self.dest.clone()));
        args.insert(
            "_original_basename".into(),
            Value::String(self.of.basename.clone()),
        );
        args.insert("follow".into(), Value::Bool(self.follow));
        if !truthy(args.get("checksum")) {
            args.insert("checksum".into(), Value::String(self.checksum.clone()));
        }
        self.state = State::Module("copy");
        Step::Run(Sub {
            module: "copy",
            args,
            files: vec![("src".into(), blob)],
        })
    }

    /// The module's result as the reference hands it back: an empty `diff` under it, the local
    /// sum when the module gave none, and `dest` for the `path` that `file` reports.
    fn finish(&self, result: TaskResult) -> TaskResult {
        let mut out = Map::new();
        out.insert("diff".into(), Value::Array(Vec::new()));
        out.extend(result.0);
        if !truthy(out.get("checksum")) {
            out.insert("checksum".into(), Value::String(self.checksum.clone()));
        }
        let mut out = TaskResult(out);
        if !out.failed()
            && !out.0.contains_key("dest")
            && let Some(path) = out.0.get("path").cloned()
        {
            out.0.insert("dest".into(), path);
        }
        out
    }
}

/// Python's truth of an optional argument: absent, `null`, `false`, `""` and `0` are false.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

impl Plugin for Copy {
    fn next(&mut self, last: Option<TaskResult>) -> Step {
        match self.state {
            State::Start => {
                self.state = State::Stat { again: false };
                stat(&self.dest_file, self.follow, self.force)
            }
            State::Stat { again } => {
                let Some(result) = last else {
                    return Step::Done(lost("stat"));
                };
                match stat_of(&self.dest_file, result) {
                    Ok(found) => self.looked_at(&found, again),
                    Err(failure) => Step::Done(failure),
                }
            }
            State::Module(module) => {
                Step::Done(last.map_or_else(|| lost(module), |r| self.finish(r)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use serde_json::{Map, Value, json};

    use super::*;
    use crate::action_plugins::{Context, Plugin, Step};

    /// A directory of this test process's own under the system's temporary directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("volant-copy-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The plugin as the driver starts it, for arguments `prepare` rendered, the names among them
    /// whose render read a managed host, and a playbook in `playbook_dir`.
    fn start_in(
        args: &Map<String, Value>,
        untrusted: &BTreeSet<String>,
        playbook_dir: &Path,
    ) -> Box<dyn Plugin> {
        let running = Map::new();
        let item_vars = crate::vars::HostVars::default();
        let templar = crate::template::Templar::new(playbook_dir.to_path_buf());
        let origin = crate::compile::Origin {
            file_dir: playbook_dir.to_path_buf(),
            ..crate::compile::Origin::default()
        };
        let mut warnings = Vec::new();
        start(Context {
            args,
            args_untrusted: untrusted,
            running_vars: &running,
            item_vars: &item_vars,
            templar: &templar,
            origin: &origin,
            playbook_dir,
            warnings: &mut warnings,
        })
    }

    fn start_copy(args: &Map<String, Value>, untrusted: &BTreeSet<String>) -> Box<dyn Plugin> {
        start_in(args, untrusted, &scratch("playbook"))
    }

    /// A `src` whose render read a managed host is refused before anything is read.
    ///
    /// Measured on ansible-core 2.19.12: a `copy` whose `src` is a registered `stdout` sends the
    /// controller file it names to the host, `~/.ssh` included, and the host reads it back with
    /// `cat`. This refuses it, on purpose.
    ///
    /// What would make this red: the source resolved before the provenance is looked at. The
    /// second path names nothing on the controller, so a lookup made first answers "Could not
    /// find or access" instead; the first is a real file whose bytes would then be hashed into a
    /// blob, one `put_blob` away from the host.
    #[test]
    fn a_source_a_host_named_is_never_read() {
        let dir = scratch("secret");
        let secret = dir.join("controller-secret");
        std::fs::write(&secret, "secret").unwrap();
        let untrusted: BTreeSet<String> = ["src".to_string()].into();
        for named in [secret.clone(), dir.join("no-such-file")] {
            let mut args = Map::new();
            args.insert("src".into(), json!(named.display().to_string()));
            args.insert("dest".into(), json!("/tmp/x"));
            let mut plugin = start_copy(&args, &untrusted);
            let Step::Done(result) = plugin.next(None) else {
                panic!("a sub-task was sent")
            };
            assert!(result.failed(), "{result:?}");
            assert_eq!(
                result.0["msg"],
                json!(
                    "the 'src' of this task was named by a managed host, and a controller file a host chose is never sent"
                ),
                "{}",
                named.display()
            );
        }
    }

    /// SHA-1 of `hello\n`, as the reference's `stat` reported it for the golden `copy-new`.
    const HELLO_SHA1: &str = "f572d396fae9206628714fb2ce00f72e94f2258f";

    /// A playbook directory holding `files/hello.txt`, and the arguments of a task copying it.
    fn hello(name: &str, extra: Value) -> (PathBuf, Map<String, Value>) {
        let dir = scratch(name);
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::write(dir.join("files/hello.txt"), "hello\n").unwrap();
        let Value::Object(mut args) = json!({"src": "hello.txt", "dest": "/tmp/v/hello.txt"})
        else {
            unreachable!()
        };
        if let Value::Object(extra) = extra {
            args.extend(extra);
        }
        (dir, args)
    }

    fn run(plugin: &mut dyn Plugin, last: Option<Value>) -> Sub {
        let last = last.map(|v| match v {
            Value::Object(map) => TaskResult(map),
            _ => unreachable!(),
        });
        match plugin.next(last) {
            Step::Run(sub) => sub,
            Step::Done(result) => panic!("done early: {result:?}"),
        }
    }

    fn done(plugin: &mut dyn Plugin, last: Value) -> Value {
        let Value::Object(last) = last else {
            unreachable!()
        };
        match plugin.next(Some(TaskResult(last))) {
            Step::Done(result) => Value::Object(result.0),
            Step::Run(sub) => panic!("another sub-task: {sub:?}"),
        }
    }

    fn copy_task(name: &str, extra: Value) -> Box<dyn Plugin> {
        let (dir, args) = hello(name, extra);
        start_in(&args, &BTreeSet::new(), &dir)
    }

    /// The first sub-task is the reference's `stat`, measured: the SHA-1 and nothing else.
    ///
    /// What would make this red: another algorithm asked for, which never equals the local sum
    /// and sends every file on every run, or the size asked for, which the reference does not.
    #[test]
    fn the_stat_is_the_reference_s() {
        let mut plugin = copy_task("stat", json!({}));
        let sub = run(plugin.as_mut(), None);
        assert_eq!(sub.module, "stat");
        assert_eq!(
            Value::Object(sub.args),
            json!({"path": "/tmp/v/hello.txt", "follow": false, "get_checksum": true,
                   "checksum_algorithm": "sha1", "get_size": false})
        );
        assert!(sub.files.is_empty());
    }

    /// A destination that already holds the bytes gets `file`, and nothing is sent.
    ///
    /// Measured on ansible-core 2.19.12: `stat` then `file`, and the result registered is
    /// `file`'s with the local sum and `dest` added (`changed checksum dest diff failed gid group
    /// mode owner path size state uid`).
    ///
    /// What would make this red: the sum taken with anything but SHA-1 over the bytes, which
    /// never matches what `stat` reports and sends the file every time, or `file` handed the
    /// source it does not take.
    #[test]
    fn an_identical_file_is_not_sent() {
        let mut plugin = copy_task("same", json!({"mode": "0644"}));
        run(plugin.as_mut(), None);
        let sub = run(
            plugin.as_mut(),
            Some(
                json!({"changed": false, "stat": {"exists": true, "isdir": false, "checksum": HELLO_SHA1}}),
            ),
        );
        assert_eq!(sub.module, "file");
        assert_eq!(
            Value::Object(sub.args),
            json!({"dest": "/tmp/v/hello.txt", "_original_basename": "hello.txt",
                   "recurse": false, "state": "file", "mode": "0644"})
        );
        assert!(sub.files.is_empty(), "nothing is staged: {:?}", sub.files);
        let result = done(
            plugin.as_mut(),
            json!({"changed": false, "path": "/tmp/v/hello.txt", "mode": "0644", "state": "file",
                   "size": 6}),
        );
        assert_eq!(
            result,
            json!({"diff": [], "changed": false, "path": "/tmp/v/hello.txt", "mode": "0644",
                   "state": "file", "size": 6, "checksum": HELLO_SHA1, "dest": "/tmp/v/hello.txt"})
        );
    }

    /// The staged `copy` the reference runs, measured: `src` is the staged file, the local sum
    /// travels as `checksum`, and the source's name as `_original_basename`.
    fn assert_sent(sub: &Sub, extra: Value) {
        assert_eq!(sub.module, "copy");
        let mut want = json!({"dest": "/tmp/v/hello.txt", "_original_basename": "hello.txt",
                              "follow": false, "checksum": HELLO_SHA1});
        if let (Value::Object(want), Value::Object(extra)) = (&mut want, extra) {
            want.extend(extra);
        }
        assert_eq!(
            Value::Object(sub.args.clone()),
            want,
            "no controller path in `src`"
        );
        assert_eq!(
            sub.files,
            [("src".to_string(), blob_of("hello.txt", b"hello\n").unwrap())]
        );
    }

    /// A destination that differs gets the bytes, once, and `copy`.
    ///
    /// What would make this red: the file not staged, which runs `copy` on a `src` nobody put
    /// there, or the controller's path left in `src`, which the host would read as its own.
    #[test]
    fn a_different_file_is_sent_once() {
        let mut plugin = copy_task("different", json!({"backup": true}));
        run(plugin.as_mut(), None);
        let zeros = "0".repeat(40);
        let sub = run(
            plugin.as_mut(),
            Some(json!({"stat": {"exists": true, "isdir": false, "checksum": zeros}})),
        );
        assert_sent(&sub, json!({"backup": true}));
        // The module's own result, with the empty `diff` the plugin puts under it.
        let result = done(
            plugin.as_mut(),
            json!({"changed": true, "checksum": HELLO_SHA1, "dest": "/tmp/v/hello.txt",
                   "src": "/staged"}),
        );
        assert_eq!(
            result,
            json!({"diff": [], "changed": true, "checksum": HELLO_SHA1,
                   "dest": "/tmp/v/hello.txt", "src": "/staged"})
        );
    }

    /// A destination that is not there gets the bytes and `copy`, as a different one does.
    ///
    /// What would make this red: a missing file compared with an empty sum, which an empty
    /// source matches and so is never created.
    #[test]
    fn an_absent_file_is_sent() {
        let mut plugin = copy_task("absent", json!({}));
        run(plugin.as_mut(), None);
        let sub = run(plugin.as_mut(), Some(json!({"stat": {"exists": false}})));
        assert_sent(&sub, json!({}));
    }

    /// `force: false` asks for no sum, and leaves a destination that exists alone.
    ///
    /// Measured on ansible-core 2.19.12: `stat` alone, and `{"changed": false, "dest": ...,
    /// "src": ...}` registered.
    ///
    /// What would make this red: the file sent anyway, which overwrites what the author asked to
    /// keep, or a sum asked for that the reference does not compute.
    #[test]
    fn force_false_leaves_an_existing_file_alone() {
        let (dir, args) = hello("force", json!({"force": false}));
        let mut plugin = start_in(&args, &BTreeSet::new(), &dir);
        let sub = run(plugin.as_mut(), None);
        assert_eq!(sub.args["get_checksum"], json!(false));
        let result = done(
            plugin.as_mut(),
            json!({"stat": {"exists": true, "isdir": false}}),
        );
        assert_eq!(
            result,
            json!({"changed": false, "dest": "/tmp/v/hello.txt",
                   "src": dir.join("files/hello.txt").display().to_string()})
        );
    }

    /// A destination that is a directory is looked at again with the source's name inside it,
    /// and the module is still handed the directory.
    ///
    /// Measured on ansible-core 2.19.12: `stat`, then `file` on `dest/<basename>`.
    ///
    /// What would make this red: the directory's own sum compared with the file's, which sends
    /// the file on every run, or `content` let into a directory.
    #[test]
    fn a_directory_dest_takes_the_basename() {
        let mut plugin = copy_task("dir", json!({"dest": "/tmp/v"}));
        run(plugin.as_mut(), None);
        let sub = run(
            plugin.as_mut(),
            Some(json!({"stat": {"exists": true, "isdir": true}})),
        );
        assert_eq!(sub.module, "stat");
        assert_eq!(sub.args["path"], json!("/tmp/v/hello.txt"));
        let sub = run(
            plugin.as_mut(),
            Some(json!({"stat": {"exists": true, "isdir": false, "checksum": HELLO_SHA1}})),
        );
        assert_eq!(sub.module, "file");
        assert_eq!(sub.args["dest"], json!("/tmp/v"));

        // A trailing `/` says it is a directory: the name inside it is the one looked at.
        let mut plugin = copy_task("slash", json!({"dest": "/tmp/v/"}));
        assert_eq!(
            run(plugin.as_mut(), None).args["path"],
            json!("/tmp/v/hello.txt")
        );

        let mut args = Map::new();
        args.insert("content".into(), json!("x"));
        args.insert("dest".into(), json!("/tmp/v"));
        let mut plugin = start_copy(&args, &BTreeSet::new());
        run(plugin.as_mut(), None);
        let result = done(
            plugin.as_mut(),
            json!({"stat": {"exists": true, "isdir": true}}),
        );
        assert_eq!(
            result["msg"],
            json!("can not use content with a dir as dest")
        );
    }

    /// `remote_src` runs `copy` with the task's arguments and nothing before it, and its `src`
    /// names a host file, which is the host's own to name.
    ///
    /// Measured on ansible-core 2.19.12: `copy` alone, no `stat` and no transfer.
    ///
    /// What would make this red: a `stat` or a transfer first, or the trust refusal applied to a
    /// path that is read on the host it came from.
    #[test]
    fn remote_src_runs_the_module_alone() {
        let Value::Object(args) =
            json!({"src": "/tmp/v/new.txt", "dest": "/tmp/v/remote.txt", "remote_src": true})
        else {
            unreachable!()
        };
        let mut plugin = start_copy(&args, &["src".to_string()].into());
        let sub = run(plugin.as_mut(), None);
        assert_eq!(sub.module, "copy");
        assert_eq!(sub.args, args);
        assert!(sub.files.is_empty());
        let result = done(
            plugin.as_mut(),
            json!({"changed": true, "src": "/tmp/v/new.txt"}),
        );
        assert_eq!(result, json!({"changed": true, "src": "/tmp/v/new.txt"}));
    }

    /// A `content` is sent as its text, a mapping as `json.dumps` writes it, and the name it
    /// travels under is a dotted one the host never writes.
    ///
    /// What would make this red: a mapping written as this engine's own JSON, which differs from
    /// what the reference writes and so changes the file on every run under the other engine.
    #[test]
    fn a_content_is_sent_as_the_reference_writes_it() {
        let mut args = Map::new();
        args.insert("content".into(), json!({"b": 1, "a": ["é", true]}));
        args.insert("dest".into(), json!("/tmp/v/c.json"));
        let mut plugin = start_copy(&args, &BTreeSet::new());
        run(plugin.as_mut(), None);
        let sub = run(plugin.as_mut(), Some(json!({"stat": {"exists": false}})));
        let text = r#"{"b": 1, "a": ["\u00e9", true]}"#;
        assert_eq!(sub.files[0].1, blob_of("x", text.as_bytes()).unwrap());
        assert_eq!(sub.args["checksum"], json!(sha1_hex(text.as_bytes())));
        assert_eq!(
            sub.args["_original_basename"],
            json!(format!(".{}", &sha1_hex(text.as_bytes())[..8]))
        );
        assert!(!sub.args.contains_key("content"), "{:?}", sub.args);
    }

    /// What is refused before any sub-task, each in the reference's words where it has some.
    ///
    /// What would make this red: a check dropped, which runs `stat` for a task that cannot
    /// succeed, or a directory `src` copied as if it were a file.
    #[test]
    fn what_cannot_be_copied_is_refused_before_the_host_is_asked() {
        let (dir, _) = hello("refused", json!({}));
        std::fs::create_dir_all(dir.join("files/tree")).unwrap();
        for (args, msg) in [
            (
                json!({"dest": "/tmp/x"}),
                "src (or content) is required".to_string(),
            ),
            (json!({"src": "hello.txt"}), "dest is required".into()),
            (
                json!({"src": "hello.txt", "content": "x", "dest": "/tmp/x"}),
                "src and content are mutually exclusive".into(),
            ),
            (
                json!({"content": "x", "dest": "/tmp/x/"}),
                "can not use content with a dir as dest".into(),
            ),
            (
                json!({"src": "tree", "dest": "/tmp/x"}),
                "copying a directory is not supported yet: tree".into(),
            ),
            (
                json!({"src": "nope.txt", "dest": "/tmp/x"}),
                format!(
                    "Unexpected AnsibleActionFail error: {}",
                    not_found(
                        "nope.txt",
                        &[
                            dir.join("files/nope.txt"),
                            dir.join("nope.txt"),
                            dir.join("files/nope.txt"),
                            dir.join("nope.txt"),
                        ]
                    )
                ),
            ),
        ] {
            let Value::Object(args) = args else {
                unreachable!()
            };
            let mut plugin = start_in(&args, &BTreeSet::new(), &dir);
            let Step::Done(result) = plugin.next(None) else {
                panic!("a sub-task was sent for {args:?}")
            };
            assert!(result.failed(), "{result:?}");
            assert_eq!(result.0["msg"], json!(msg));
        }
    }

    /// A `stat` that fails ends the task in the reference's words, and a `copy` that fails is
    /// the task's result as the module gave it.
    ///
    /// Measured on ansible-core 2.19.12, a failing `validate` registers `{"changed": false,
    /// "checksum": "da39...", "exit_status": 1, "msg": "failed to validate", ...}`.
    ///
    /// What would make this red: a failed `stat` read as a missing file, which sends the file
    /// into a directory the host cannot read, or the module's failure rewritten.
    #[test]
    fn a_failed_sub_task_ends_the_task() {
        let mut plugin = copy_task("stat-failed", json!({}));
        run(plugin.as_mut(), None);
        let result = done(
            plugin.as_mut(),
            json!({"failed": true, "msg": "Permission denied"}),
        );
        assert_eq!(result["failed"], json!(true));
        assert_eq!(
            result["msg"],
            json!("Failed to get information on remote file (/tmp/v/hello.txt): Permission denied")
        );

        let mut plugin = copy_task("validate", json!({"validate": "test -s %s"}));
        run(plugin.as_mut(), None);
        run(plugin.as_mut(), Some(json!({"stat": {"exists": false}})));
        let failed = json!({"changed": false, "checksum": "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "exit_status": 1, "failed": true, "msg": "failed to validate", "stderr": "",
            "stderr_lines": [], "stdout": "", "stdout_lines": []});
        let result = done(plugin.as_mut(), failed.clone());
        let Value::Object(mut want) = failed else {
            unreachable!()
        };
        want.insert("diff".into(), json!([]));
        assert_eq!(result, Value::Object(want));
    }
}
