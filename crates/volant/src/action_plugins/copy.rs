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

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::encoding::sha1_hex;

use super::files::{blob_of, fits, not_found, refuse_host_named, search_paths};
use super::{Context, Plugin, Step, Sub, lost};
use crate::executor::as_bool_value;

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

/// Whether the item is one the reference's own plugin answers: a copy of a controller directory,
/// which this one does not do, or of an absolute path that is not there, whose failure the
/// reference shapes as a result of its own (`changed`, `exception`, and no search list).
pub(super) fn unwritten(ctx: &Context<'_>) -> bool {
    let args = ctx.args;
    let Some(src) = text(args, "src") else {
        return false;
    };
    if !args.get("content").is_none_or(Value::is_null) || flag(args, "remote_src", false) {
        return false;
    }
    let searched = search_paths(ctx.origin, ctx.playbook_dir, "files", src);
    match searched.iter().find(|p| p.exists()) {
        Some(found) => found.is_dir(),
        None => std::path::Path::new(src).is_absolute(),
    }
}

pub(super) fn start(ctx: Context<'_>) -> Box<dyn Plugin> {
    match source(&ctx) {
        Ok(Some(of)) => copy_bytes(of),
        // Measured: with `remote_src` the module runs alone, with the task's arguments as they
        // are, and no `stat` before it.
        Ok(None) => Box::new(Then(Some(Sub::run("copy", ctx.args.clone())))),
        Err(msg) => failing(msg),
    }
}

/// A plugin that fails the item with `msg` before the host is asked anything.
pub(super) fn failing(msg: String) -> Box<dyn Plugin> {
    Box::new(Then(Some(Step::Done(TaskResult::failed_with(msg)))))
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
    refuse_host_named(ctx.args_untrusted, "src")?;
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
    let unreadable = |err: std::io::Error| format!("could not read src={}: {err}", found.display());
    let basename = found
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Before the read, so a file no frame can carry is never loaded whole. Refused whatever the
    // host holds, where the reference would still run `file` on a destination already right.
    let len = std::fs::metadata(found).map_err(unreadable)?.len();
    fits(&basename, usize::try_from(len).unwrap_or(usize::MAX))?;
    let mut bytes = std::fs::read(found).map_err(unreadable)?;
    // A vaulted source is sent as plain text unless `decrypt: false` asks for it as it is.
    if flag(args, "decrypt", true) && crate::vault::is_encrypted(&bytes) {
        bytes = crate::vault::decrypt(&bytes, &crate::vault::run_secrets())
            .map_err(|err| format!("could not read src={}: {err}", found.display()))?;
    }
    let mut args = args.clone();
    args.insert("src".into(), Value::String(found.display().to_string()));
    // Read off the local file, as the reference does. Left to the module, `preserve` would
    // read the mode of the private copy the agent staged, and set that.
    if args.get("mode").and_then(Value::as_str) == Some("preserve") {
        args.insert("mode".into(), Value::String(local_mode(found)?));
    }
    Ok(Some(CopyOf {
        bytes,
        basename,
        args,
    }))
}

/// `mode: preserve` read off a controller file, as the reference writes it: `0%03o`.
#[cfg(unix)]
pub(super) fn local_mode(path: &std::path::Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|err| format!("could not read the mode of {}: {err}", path.display()))?
        .permissions()
        .mode();
    Ok(format!("0{:03o}", mode & 0o7777))
}

#[cfg(not(unix))]
pub(super) fn local_mode(path: &std::path::Path) -> Result<String, String> {
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
        // `json.dumps` with its defaults, `ensure_ascii` among them.
        other => crate::template::python_json(other, None, false, true, 0),
    }
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

/// The `stat` the reference runs, measured: a SHA-1 unless `force` is off, in which case a
/// destination that exists is left alone whatever it holds.
///
/// Sends only what the `stat` module's own `argument_spec` declares (read off
/// `ansible/modules/stat.py` of ansible-core 2.19.12: `path`, `follow`, `get_checksum`,
/// `get_mime`, `get_attributes`, `checksum_algorithm`). The reference's own controller-side stat
/// call also sends `get_size: false`, a key `ansible.windows.win_stat` added and the posix module
/// never declared; that call tolerates an argument the module does not know, and a sub-task run
/// through this dispatch does not, so `get_size` stays out.
///
/// `get_mime` and `get_attributes` are sent `false`, where the reference leaves them at their
/// defaults: no plugin reads `mimetype`, `charset` or `attributes`, and the defaults run `file`
/// and `lsattr` on the host at every call (about 5 ms against 1 ms, measured on the native
/// `stat`). The only trace is this sub-task's own `invocation`, which neither engine shows or
/// hands on.
pub(super) fn stat(path: &str, follow: bool, checksum: bool) -> Step {
    let mut args = Map::new();
    args.insert("path".into(), Value::String(path.to_string()));
    args.insert("follow".into(), Value::Bool(follow));
    args.insert("get_checksum".into(), Value::Bool(checksum));
    args.insert("get_mime".into(), Value::Bool(false));
    args.insert("get_attributes".into(), Value::Bool(false));
    args.insert("checksum_algorithm".into(), Value::String("sha1".into()));
    Sub::run("stat", args)
}

/// The `stat` block of a `stat` result, or the reference's failure when there is none.
pub(super) fn stat_of(path: &str, result: TaskResult) -> Result<Map<String, Value>, TaskResult> {
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
mod tests;
