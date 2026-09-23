// SPDX-License-Identifier: GPL-3.0-or-later
//! `fetch`: a file of the host, written onto the controller under `dest` and nowhere else.
//!
//! Read off `plugins/action/fetch.py` of ansible-core 2.19.12 and measured against it. Without
//! `become` the plugin runs `stat` on the source and leaves a local file with the same SHA-1
//! alone; under `become` it runs `slurp` alone. The bytes always come from `slurp` here, where the
//! reference copies them over its connection instead: this engine's link carries module results
//! and nothing else. They are a file and never a value: nothing renders them, and they reach no
//! variable.
//!
//! Two refusals are this engine's own, and stricter than the reference:
//!
//! - A `dest` whose render read a managed host is refused. A host never chooses where the
//!   controller writes.
//! - The path written is built, normalised without following anything, and refused unless it sits
//!   under `dest` (and under `dest/<inventory_hostname>/` without `flat`). Measured on
//!   ansible-core 2.19.12, the reference checks the containment before it builds the path, so a
//!   relative `src` of `../../../../../../../../tmp/x` without `flat` writes `/tmp/x` on the
//!   controller. A symbolic link below `dest` is refused as well, so a link planted inside it
//!   cannot send the write elsewhere.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::encoding::{b64_decode, sha1_hex};

use super::copy::{failing, stat, stat_of};
use super::{Context, Plugin, Step, Sub, lost};
use crate::executor::as_bool_value;

/// The reference's failure when the source is a directory, from `stat` or from `slurp`.
const IS_A_DIRECTORY: &str = "remote file is a directory, fetch cannot work on directories";

pub(super) fn start(ctx: Context<'_>) -> Box<dyn Plugin> {
    match Fetch::new(&ctx) {
        Ok(fetch) => Box::new(fetch),
        Err(msg) => failing(msg),
    }
}

enum State {
    Start,
    Stat,
    Slurp,
}

struct Fetch {
    /// The task's `src`, as the playbook wrote it.
    src: String,
    /// The task's `dest`, as the playbook wrote it, for the messages.
    dest: String,
    /// `dest` made absolute and normalised: nothing is written anywhere but below it.
    root: PathBuf,
    /// `inventory_hostname`, one path component; unused under `flat`.
    host: String,
    flat: bool,
    fail_on_missing: bool,
    validate_checksum: bool,
    escalated: bool,
    /// The source as the host named it back, `~` and variables expanded.
    source: String,
    /// The SHA-1 `stat` reported, when it reported one.
    remote_checksum: Option<String>,
    state: State,
}

fn flag(args: &Map<String, Value>, name: &str, default: bool) -> bool {
    args.get(name).and_then(as_bool_value).unwrap_or(default)
}

impl Fetch {
    fn new(ctx: &Context<'_>) -> Result<Self, String> {
        let args = ctx.args;
        let arg = |name| args.get(name).filter(|v| !v.is_null());
        // The reference's checks, in its order and in its words: the last one that applies wins.
        let mut msg = None;
        if !matches!(arg("src"), Some(Value::String(_))) {
            msg = Some("Invalid type supplied for source option, it must be a string");
        }
        if !matches!(arg("dest"), Some(Value::String(_))) {
            msg = Some("Invalid type supplied for dest option, it must be a string");
        }
        if arg("src").is_none() || arg("dest").is_none() {
            msg = Some("src and dest are required");
        }
        if let Some(msg) = msg {
            return Err(msg.into());
        }
        if ctx.args_untrusted.contains("dest") {
            return Err("the 'dest' of this task was named by a managed host, and a controller path a host chose is never written".into());
        }
        let text = |name| {
            arg(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (src, dest) = (text("src"), text("dest"));
        let flat = flag(args, "flat", false);
        let host = ctx
            .item_vars
            .map
            .get("inventory_hostname")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Without `flat` the host's name is a directory under `dest`: `..` or a `/` in it would
        // put one host's files outside its own directory.
        let mut parts = Path::new(&host).components();
        if !flat
            && !matches!(
                (parts.next(), parts.next()),
                (Some(Component::Normal(_)), None)
            )
        {
            return Err(format!(
                "the inventory_hostname '{host}' is not one path component, and fetch files what it fetches under it"
            ));
        }
        let fetch = Self {
            root: normalise(&absolute(&dest, ctx.playbook_dir)),
            source: src.clone(),
            src,
            dest,
            host,
            flat,
            fail_on_missing: flag(args, "fail_on_missing", true),
            validate_checksum: flag(args, "validate_checksum", true),
            escalated: ctx.escalated,
            remote_checksum: None,
            state: State::Start,
        };
        // Once before the host is asked anything, on the path as the playbook wrote it, and again
        // on the one the host names back.
        fetch.place(&fetch.src)?;
        Ok(fetch)
    }

    /// The controller path the source is written to, or the refusal. Lexical only.
    fn place(&self, source: &str) -> Result<PathBuf, String> {
        let root = self.root.display();
        let (joined, within) = if !self.flat {
            (
                format!("{root}/{}/{source}", self.host),
                self.root.join(&self.host),
            )
        } else if self.dest.ends_with('/') {
            let base = self.src.rsplit('/').next().unwrap_or_default();
            (format!("{root}/{base}"), self.root.clone())
        } else {
            return Ok(self.root.clone());
        };
        let path = normalise(Path::new(&joined));
        if !path.starts_with(&within) || path == within {
            return Err(format!(
                "the fetched path {} is outside '{}'",
                path.display(),
                self.dest
            ));
        }
        Ok(path)
    }

    /// [`Self::place`], then what the file system says about it: a `flat` destination that is a
    /// directory, and a symbolic link anywhere below `dest`.
    fn target(&self, source: &str) -> Result<PathBuf, String> {
        let path = self.place(source)?;
        if self.flat && !self.dest.ends_with('/') && path.is_dir() {
            return Err("dest is an existing directory, use a trailing slash if you want to fetch src into that directory".into());
        }
        walk(self.walk_root(), &path, false)?;
        Ok(path)
    }

    /// Where the walk for links starts: `dest`, or its parent when `dest` is the file itself.
    fn walk_root(&self) -> &Path {
        if self.flat && !self.dest.ends_with('/') {
            self.root.parent().unwrap_or(&self.root)
        } else {
            &self.root
        }
    }

    /// The first answer of the host: `stat` without `become`.
    fn stated(&mut self, result: TaskResult) -> Step {
        let found = match stat_of(&self.src, result) {
            Ok(found) => found,
            Err(failure) => {
                let msg = failure.0.get("msg").and_then(Value::as_str).unwrap_or("");
                return Step::Done(self.missing(if self.fail_on_missing {
                    msg.to_string()
                } else {
                    format!("{msg}, ignored")
                }));
            }
        };
        if found.get("isdir").and_then(Value::as_bool) == Some(true) {
            return Step::Done(TaskResult::failed_with(IS_A_DIRECTORY));
        }
        let exists = found.get("exists").and_then(Value::as_bool) == Some(true);
        if let Some(path) = found.get("path").and_then(Value::as_str) {
            self.source = path.to_string();
        }
        let checksum = found.get("checksum").and_then(Value::as_str);
        // What the reference reads as "no sum": a missing file, or one `stat` could not sum.
        let Some(remote) = checksum.filter(|c| exists && !c.is_empty()) else {
            return self.slurp();
        };
        self.remote_checksum = Some(remote.to_string());
        match self.target(&self.source) {
            Ok(path) => self
                .unchanged(&path, remote)
                .map_or_else(|| self.slurp(), Step::Done),
            Err(msg) => Step::Done(TaskResult::failed_with(msg)),
        }
    }

    fn slurp(&mut self) -> Step {
        self.state = State::Slurp;
        let mut args = Map::new();
        args.insert("src".into(), Value::String(self.src.clone()));
        Sub::run("slurp", args)
    }

    /// The result when the local file already has the remote sum.
    fn unchanged(&self, path: &Path, remote: &str) -> Option<TaskResult> {
        let local = std::fs::read(path).ok().map(|b| sha1_hex(&b));
        if local.as_deref() != Some(remote) {
            return None;
        }
        // Measured: `changed: false`, the local sum, and no remote one.
        let mut out = Map::new();
        out.insert("changed".into(), Value::Bool(false));
        out.insert("checksum".into(), Value::String(remote.to_string()));
        out.insert("dest".into(), Value::String(path.display().to_string()));
        out.insert("file".into(), Value::String(self.source.clone()));
        out.insert("md5sum".into(), Value::Null);
        out.insert("remote_checksum".into(), Value::Null);
        Some(TaskResult(out))
    }

    /// The result of a source that is not there, or that `stat` could not look at.
    fn missing(&self, msg: String) -> TaskResult {
        let mut out = Map::new();
        out.insert("changed".into(), Value::Bool(false));
        out.insert("file".into(), Value::String(self.source.clone()));
        if self.fail_on_missing {
            out.insert("failed".into(), Value::Bool(true));
        }
        out.insert("msg".into(), Value::String(msg));
        TaskResult(out)
    }

    /// The `slurp` result: the bytes, or the reference's reading of its failure.
    fn slurped(&mut self, result: TaskResult) -> TaskResult {
        if result.failed() {
            let msg = result.0.get("msg").and_then(Value::as_str).unwrap_or("");
            let msg = if msg.contains("not found") {
                "the remote file does not exist, not transferring, ignored".to_string()
            } else if msg.starts_with("source is a directory") {
                IS_A_DIRECTORY.to_string()
            } else {
                msg.to_string()
            };
            // The module's own result under `fail_on_missing`, as the reference hands it back;
            // an agent that refused an oversized answer is named there as it said it.
            let mut out = if self.fail_on_missing {
                result
            } else {
                self.missing(String::new())
            };
            out.0.insert("msg".into(), Value::String(msg));
            return out;
        }
        let data = match (
            result.0.get("encoding").and_then(Value::as_str),
            result.0.get("content").and_then(Value::as_str),
        ) {
            (Some("base64"), Some(content)) => match b64_decode(content) {
                Ok(data) => data,
                Err(err) => return TaskResult::failed_with(format!("slurp content: {err}")),
            },
            _ => return TaskResult::failed_with("slurp returned no base64 content"),
        };
        if let Some(source) = result.0.get("source").and_then(Value::as_str) {
            self.source = source.to_string();
        }
        let remote = self
            .remote_checksum
            .clone()
            .unwrap_or_else(|| sha1_hex(&data));
        match self.target(&self.source) {
            Ok(path) => self
                .unchanged(&path, &remote)
                .unwrap_or_else(|| self.write(path, &data, &remote)),
            Err(msg) => TaskResult::failed_with(msg),
        }
    }

    /// The bytes written next to their place, checked, then renamed over it.
    fn write(&self, path: PathBuf, data: &[u8], remote: &str) -> TaskResult {
        let mut tmp = path.clone().into_os_string();
        tmp.push(".volant-tmp");
        let tmp = PathBuf::from(tmp);
        let written = walk(self.walk_root(), &path, true).and_then(|()| {
            let fail = |err: std::io::Error| format!("Failed to fetch the file: {err}");
            // A leftover of an interrupted run; removing it never follows a link.
            let _ = std::fs::remove_file(&tmp);
            let mut file = std::fs::File::create_new(&tmp).map_err(fail)?;
            file.write_all(data).map_err(fail)?;
            drop(file);
            std::fs::read(&tmp).map(|b| sha1_hex(&b)).map_err(fail)
        });
        let new = match written {
            Ok(new) => new,
            Err(msg) => {
                let _ = std::fs::remove_file(&tmp);
                return TaskResult::failed_with(msg);
            }
        };
        let dest = Value::String(path.display().to_string());
        let mut out = Map::new();
        if self.validate_checksum && new != remote {
            // Unlike the reference, which leaves the mismatched file in place, nothing is written.
            let _ = std::fs::remove_file(&tmp);
            out.insert("failed".into(), Value::Bool(true));
            out.insert("msg".into(), Value::String("checksum mismatch".into()));
            out.insert("file".into(), Value::String(self.source.clone()));
        } else if let Err(err) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return TaskResult::failed_with(format!("Failed to fetch the file: {err}"));
        } else {
            out.insert("changed".into(), Value::Bool(true));
        }
        out.insert("checksum".into(), Value::String(new));
        out.insert("dest".into(), dest);
        // No MD5 here; the reference too reports `None` where it cannot compute one.
        out.insert("md5sum".into(), Value::Null);
        out.insert("remote_checksum".into(), Value::String(remote.to_string()));
        out.insert("remote_md5sum".into(), Value::Null);
        TaskResult(out)
    }
}

impl Plugin for Fetch {
    fn next(&mut self, last: Option<TaskResult>) -> Step {
        match self.state {
            // Measured: under `become` the reference runs `slurp` alone, and no `stat`.
            State::Start if self.escalated => self.slurp(),
            State::Start => {
                self.state = State::Stat;
                stat(&self.src, true, true)
            }
            State::Stat => match last {
                Some(result) => self.stated(result),
                None => Step::Done(lost("stat")),
            },
            State::Slurp => Step::Done(last.map_or_else(|| lost("slurp"), |r| self.slurped(r))),
        }
    }
}

/// `dest` as the reference reads it: `~` for the controller's home, relative to the playbook.
fn absolute(dest: &str, playbook_dir: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (dest.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(format!("{}{rest}", home.display()))
        }
        _ => playbook_dir.join(dest),
    }
}

/// A path with `.` dropped and `..` taken off the component before it, and nothing looked up:
/// a link in it is not followed, it is refused later by [`walk`].
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `/..` is `/`, as `os.path.normpath` has it.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other),
        }
    }
    out
}

/// Every component of `path` below `root`, refused when it is a symbolic link; with `create`, the
/// directories that are missing made one by one on the way down.
fn walk(root: &Path, path: &Path, create: bool) -> Result<(), String> {
    let fail = |err: std::io::Error| format!("Failed to fetch the file: {err}");
    if create {
        std::fs::create_dir_all(root).map_err(fail)?;
    }
    let below: Vec<Component<'_>> = path
        .strip_prefix(root)
        .map_err(|_| {
            format!(
                "the fetched path {} is outside '{}'",
                path.display(),
                root.display()
            )
        })?
        .components()
        .collect();
    let mut at = root.to_path_buf();
    for (i, part) in below.iter().enumerate() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "the fetched path {} goes through the symbolic link {}, and fetch never follows one",
                    path.display(),
                    at.display()
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if create && i + 1 < below.len() {
                    std::fs::create_dir(&at).map_err(fail)?;
                }
            }
            // A component that is a file: nothing below it can exist, and the write says so.
            Err(_) if !create => return Ok(()),
            Err(err) => return Err(fail(err)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;
    use volant_protocol::encoding::b64_encode;

    use super::*;
    use crate::vars::HostVars;

    /// A fresh directory of this test process's own under the system's temporary directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("volant-fetch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn start_with(
        args: Value,
        untrusted: &BTreeSet<String>,
        host: &str,
        escalated: bool,
    ) -> Box<dyn Plugin> {
        let Value::Object(args) = args else {
            unreachable!()
        };
        let running = Map::new();
        let mut item_vars = HostVars::default();
        item_vars.insert("inventory_hostname".into(), json!(host));
        let dir = std::env::temp_dir();
        let templar = crate::template::Templar::new(dir.clone());
        let origin = crate::compile::Origin::default();
        let mut warnings = Vec::new();
        start(Context {
            args: &args,
            args_untrusted: untrusted,
            running_vars: &running,
            delegated: false,
            escalated,
            item_vars: &item_vars,
            templar: &templar,
            origin: &origin,
            playbook_dir: &dir,
            warnings: &mut warnings,
        })
    }

    fn start_plain(args: Value) -> Box<dyn Plugin> {
        start_with(args, &BTreeSet::new(), "probe-hostname", false)
    }

    fn result(value: Value) -> TaskResult {
        let Value::Object(map) = value else {
            unreachable!()
        };
        TaskResult(map)
    }

    /// The plugin run to its end against a host holding `data` at `reported`, the path the host
    /// names back: each sub-task it asks for, and the result it ends with.
    fn drive(plugin: &mut dyn Plugin, data: &[u8], reported: &str) -> (Vec<Sub>, Value) {
        let mut subs = Vec::new();
        let mut last = None;
        loop {
            match plugin.next(last.take()) {
                Step::Done(result) => return (subs, Value::Object(result.0)),
                Step::Run(sub) => {
                    last = Some(result(match sub.module {
                        "stat" => json!({"changed": false, "stat": {"exists": true,
                            "isdir": false, "path": reported, "checksum": sha1_hex(data)}}),
                        "slurp" => json!({"changed": false, "content": b64_encode(data),
                            "source": reported, "encoding": "base64"}),
                        other => panic!("unexpected sub-task {other}"),
                    }));
                    subs.push(sub);
                }
            }
        }
    }

    fn modules(subs: &[Sub]) -> Vec<&str> {
        subs.iter().map(|s| s.module).collect()
    }

    /// A `dest` whose render read a managed host is refused before the host is asked anything,
    /// and nothing is created under it.
    ///
    /// The counterpart of `copy`'s refusal of a host-named `src`: a host never chooses where the
    /// controller writes. What would make this red: the provenance looked at after the path is
    /// built, which runs `stat` and `slurp` and creates `dest/probe-hostname/` for a path a host
    /// chose.
    #[test]
    fn a_dest_a_host_named_is_never_written() {
        let dir = scratch("host-named");
        let dest = dir.join("chosen");
        let untrusted: BTreeSet<String> = ["dest".to_string()].into();
        for escalated in [false, true] {
            let mut plugin = start_with(
                json!({"src": "/etc/one.txt", "dest": dest.display().to_string()}),
                &untrusted,
                "probe-hostname",
                escalated,
            );
            let (subs, result) = drive(plugin.as_mut(), b"x", "/etc/one.txt");
            assert!(subs.is_empty(), "the host was asked: {:?}", modules(&subs));
            assert_eq!(result["failed"], json!(true));
            assert_eq!(
                result["msg"],
                json!(
                    "the 'dest' of this task was named by a managed host, and a controller path a host chose is never written"
                )
            );
            assert!(!dest.exists(), "{} was created", dest.display());
        }
    }

    /// The relative `src` that climbs out of `dest` is refused, and nothing is written where it
    /// points.
    ///
    /// Measured on ansible-core 2.19.12: `src: ../../../../../../../../tmp/x/esc.txt` without
    /// `flat` reported `changed` with `dest: /tmp/x/esc.txt`, and the file was there on the
    /// controller. The marker is that file's counterpart here, under the scratch directory. What
    /// would make this red: the containment checked on `dest` before the path is built, as the
    /// reference does, which writes the marker.
    #[test]
    fn a_src_that_climbs_out_of_dest_is_refused_and_nothing_is_written() {
        let dir = scratch("climb");
        let marker = dir.join("esc.txt");
        let dest = dir.join("fetched");
        let climb = format!(
            "{}{}",
            "../".repeat(dest.components().count() + 8),
            marker.display().to_string().trim_start_matches('/')
        );
        for escalated in [false, true] {
            let mut plugin = start_with(
                json!({"src": climb, "dest": dest.display().to_string()}),
                &BTreeSet::new(),
                "probe-hostname",
                escalated,
            );
            let (_, result) = drive(plugin.as_mut(), b"escaped", &climb);
            assert_eq!(result["failed"], json!(true), "{result}");
            assert_eq!(
                result["msg"],
                json!(format!(
                    "the fetched path {} is outside '{}'",
                    marker.display(),
                    dest.display()
                ))
            );
            assert!(!marker.exists(), "{} was written", marker.display());
            assert!(!dest.exists(), "{} was created", dest.display());
        }
    }

    /// A path the host names back that climbs out of its own directory is refused the same way,
    /// even when the playbook's `src` did not climb.
    ///
    /// What would make this red: the containment checked only on the playbook's `src`, which
    /// writes wherever a host's `stat` or `slurp` says the file is.
    #[test]
    fn a_path_the_host_names_back_cannot_climb_out_either() {
        let dir = scratch("reported");
        let dest = dir.join("fetched");
        // Out of `probe-hostname/`, into a sibling host's directory: still under `dest`, and
        // still refused.
        for (escalated, reported) in [(false, "../other/one.txt"), (true, "../../esc.txt")] {
            let mut plugin = start_with(
                json!({"src": "one.txt", "dest": dest.display().to_string()}),
                &BTreeSet::new(),
                "probe-hostname",
                escalated,
            );
            let (_, result) = drive(plugin.as_mut(), b"x", reported);
            assert_eq!(result["failed"], json!(true), "{result}");
            let msg = result["msg"].as_str().unwrap();
            assert!(msg.starts_with("the fetched path "), "{msg}");
            assert!(
                msg.ends_with(&format!(" is outside '{}'", dest.display())),
                "{msg}"
            );
        }
        assert!(!dest.join("other").exists());
        assert!(!dir.join("esc.txt").exists());
    }

    /// An `inventory_hostname` that is not one path component is refused before anything, and
    /// `flat`, which does not use it, is not.
    ///
    /// What would make this red: the name joined as it is, which files `a/b`'s fetches under
    /// host `a`'s directory and `..`'s beside `dest`.
    #[test]
    fn a_host_name_that_is_not_one_path_component_is_refused() {
        let dir = scratch("names");
        let dest = dir.join("fetched");
        for host in ["a/b", "..", ".", ""] {
            let mut plugin = start_with(
                json!({"src": "/etc/one.txt", "dest": dest.display().to_string()}),
                &BTreeSet::new(),
                host,
                false,
            );
            let (subs, result) = drive(plugin.as_mut(), b"x", "/etc/one.txt");
            assert!(subs.is_empty(), "{host}: {:?}", modules(&subs));
            assert_eq!(
                result["msg"],
                json!(format!(
                    "the inventory_hostname '{host}' is not one path component, and fetch files what it fetches under it"
                ))
            );
        }
        assert!(!dest.exists());
        let flat = dir.join("flat.txt");
        let mut plugin = start_with(
            json!({"src": "/etc/one.txt", "dest": flat.display().to_string(), "flat": true}),
            &BTreeSet::new(),
            "a/b",
            false,
        );
        let (_, result) = drive(plugin.as_mut(), b"x", "/etc/one.txt");
        assert_eq!(result["changed"], json!(true), "{result}");
        assert_eq!(std::fs::read(&flat).unwrap(), b"x");
    }

    /// A symbolic link planted below `dest` never redirects the write: the host's directory, a
    /// directory under it, or a `flat` file that is a link, each refused, and what the link points
    /// at left as it was.
    ///
    /// What would make this red: the directories made with `create_dir_all` and the file opened
    /// through the link, which writes `outside/etc/one.txt` and overwrites `outside/target.txt`.
    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_dest_never_redirects_the_write() {
        use std::os::unix::fs::symlink;
        let dir = scratch("links");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("target.txt"), "kept").unwrap();

        let dest = dir.join("by-host");
        std::fs::create_dir_all(&dest).unwrap();
        symlink(&outside, dest.join("probe-hostname")).unwrap();
        let deeper = dir.join("deeper");
        std::fs::create_dir_all(deeper.join("probe-hostname")).unwrap();
        symlink(&outside, deeper.join("probe-hostname/etc")).unwrap();
        let flat = dir.join("flat");
        std::fs::create_dir_all(&flat).unwrap();
        symlink(outside.join("target.txt"), flat.join("one.txt")).unwrap();

        for (args, link) in [
            (
                json!({"src": "/etc/one.txt", "dest": dest.display().to_string()}),
                dest.join("probe-hostname"),
            ),
            (
                json!({"src": "/etc/one.txt", "dest": deeper.display().to_string()}),
                deeper.join("probe-hostname/etc"),
            ),
            (
                json!({"src": "/etc/one.txt", "dest": format!("{}/", flat.display()), "flat": true}),
                flat.join("one.txt"),
            ),
            (
                json!({"src": "/etc/one.txt", "dest": flat.join("one.txt").display().to_string(), "flat": true}),
                flat.join("one.txt"),
            ),
        ] {
            for escalated in [false, true] {
                let mut plugin =
                    start_with(args.clone(), &BTreeSet::new(), "probe-hostname", escalated);
                let (_, result) = drive(plugin.as_mut(), b"planted", "/etc/one.txt");
                assert_eq!(result["failed"], json!(true), "{args}: {result}");
                let msg = result["msg"].as_str().unwrap();
                assert!(
                    msg.ends_with(&format!(
                        " goes through the symbolic link {}, and fetch never follows one",
                        link.display()
                    )),
                    "{msg}"
                );
            }
        }
        assert!(
            !outside.join("etc").exists(),
            "a directory was made through the link"
        );
        assert_eq!(std::fs::read(outside.join("target.txt")).unwrap(), b"kept");
    }

    /// The `stat` the reference runs, the `slurp` after it, and the file written under
    /// `dest/<inventory_hostname>/<src>` with the measured result; then, the file being there,
    /// `stat` alone and `changed: false`.
    ///
    /// Measured on ansible-core 2.19.12: the first run `changed` with `dest:
    /// <dest>/<host>/tmp/x/d/one.txt` and `checksum` equal to `remote_checksum`; the second
    /// `ok`, `changed: false`, `file: <src>`, `remote_checksum: null`. The content is a template
    /// that is written as it is. What would make this red: the local sum not compared, which
    /// slurps and rewrites the file on every run, or the bytes rendered, which writes `2`.
    #[test]
    fn a_file_is_written_under_its_host_and_left_alone_once_there() {
        let dir = scratch("written");
        let dest = dir.join("fetched");
        let data = b"{{ 1 + 1 }}\n";
        let sum = sha1_hex(data);
        let args = json!({"src": "/tmp/v/d/one.txt", "dest": dest.display().to_string()});
        let file = dest.join("probe-hostname/tmp/v/d/one.txt");

        let mut plugin = start_plain(args.clone());
        let (subs, result) = drive(plugin.as_mut(), data, "/tmp/v/d/one.txt");
        assert_eq!(modules(&subs), ["stat", "slurp"]);
        assert_eq!(
            Value::Object(subs[0].args.clone()),
            json!({"path": "/tmp/v/d/one.txt", "follow": true, "get_checksum": true,
                   "checksum_algorithm": "sha1"})
        );
        assert_eq!(
            Value::Object(subs[1].args.clone()),
            json!({"src": "/tmp/v/d/one.txt"})
        );
        assert_eq!(
            result,
            json!({"changed": true, "checksum": sum, "dest": file.display().to_string(),
                   "md5sum": null, "remote_checksum": sum, "remote_md5sum": null})
        );
        assert_eq!(std::fs::read(&file).unwrap(), data);
        let mut tmp = file.clone().into_os_string();
        tmp.push(".volant-tmp");
        assert!(!Path::new(&tmp).exists(), "the temporary file was left");

        let mut plugin = start_plain(args);
        let (subs, result) = drive(plugin.as_mut(), data, "/tmp/v/d/one.txt");
        assert_eq!(modules(&subs), ["stat"]);
        assert_eq!(
            result,
            json!({"changed": false, "checksum": sum, "dest": file.display().to_string(),
                   "file": "/tmp/v/d/one.txt", "md5sum": null, "remote_checksum": null})
        );
    }

    /// Under `become`, `slurp` alone, and the sum taken off its bytes.
    ///
    /// Measured on ansible-core 2.19.12: `slurp` and no `stat`, `changed`. What would make this
    /// red: a `stat` first, which the reference does not send.
    #[test]
    fn under_become_the_file_is_slurped_without_a_stat() {
        let dir = scratch("become");
        let dest = dir.join("fetched");
        let args = json!({"src": "/etc/k.yaml", "dest": dest.display().to_string()});
        let mut plugin = start_with(args.clone(), &BTreeSet::new(), "probe-hostname", true);
        let (subs, result) = drive(plugin.as_mut(), b"secret", "/etc/k.yaml");
        assert_eq!(modules(&subs), ["slurp"]);
        assert_eq!(result["changed"], json!(true), "{result}");
        assert_eq!(result["remote_checksum"], json!(sha1_hex(b"secret")));
        let mut plugin = start_with(args, &BTreeSet::new(), "probe-hostname", true);
        let (_, result) = drive(plugin.as_mut(), b"secret", "/etc/k.yaml");
        assert_eq!(result["changed"], json!(false), "{result}");
    }

    /// `flat` writes `dest` itself, or the source's name inside a `dest` that ends with `/`; a
    /// `dest` that is an existing directory without the `/` is refused in the reference's words.
    ///
    /// Measured on ansible-core 2.19.12: `flat: true` with a `dest` ending in `/` reported `dest:
    /// <dest>/one.txt`. What would make this red: the host's directory added under `flat`, or a
    /// directory `dest` replaced by the file.
    #[test]
    fn flat_writes_dest_or_the_name_inside_it() {
        let dir = scratch("flat");
        let into = format!("{}/", dir.join("into").display());
        let mut plugin =
            start_plain(json!({"src": "/tmp/v/d/one.txt", "dest": into, "flat": true}));
        let (_, result) = drive(plugin.as_mut(), b"one", "/tmp/v/d/one.txt");
        assert_eq!(
            result["dest"],
            json!(dir.join("into/one.txt").display().to_string())
        );
        assert_eq!(std::fs::read(dir.join("into/one.txt")).unwrap(), b"one");

        let mut plugin = start_plain(
            json!({"src": "/tmp/v/d/one.txt", "dest": dir.join("into").display().to_string(),
                   "flat": "yes"}),
        );
        let (_, result) = drive(plugin.as_mut(), b"two", "/tmp/v/d/one.txt");
        assert_eq!(
            result["msg"],
            json!(
                "dest is an existing directory, use a trailing slash if you want to fetch src into that directory"
            )
        );
        assert!(dir.join("into").is_dir());
    }

    /// A source that is not there follows `fail_on_missing`, and a directory is refused.
    ///
    /// Measured on ansible-core 2.19.12: `stat`, then `slurp`, and with `fail_on_missing: false`
    /// `ok` with `msg: the remote file does not exist, not transferring, ignored`. What would make
    /// this red: a missing file written as an empty one, or a directory slurped.
    #[test]
    fn a_missing_source_follows_fail_on_missing_and_a_directory_fails() {
        let dir = scratch("missing");
        let dest = dir.join("fetched").display().to_string();
        let not_found = json!({"failed": true, "msg": "file not found: /nope"});
        for (fail_on_missing, failed) in [(false, None), (true, Some(json!(true)))] {
            let mut plugin = start_plain(
                json!({"src": "/nope", "dest": dest, "fail_on_missing": fail_on_missing}),
            );
            assert!(matches!(
                plugin.next(None),
                Step::Run(Sub { module: "stat", .. })
            ));
            let Step::Run(sub) = plugin.next(Some(result(json!({"stat": {"exists": false}}))))
            else {
                panic!("no slurp")
            };
            assert_eq!(sub.module, "slurp");
            let Step::Done(done) = plugin.next(Some(result(not_found.clone()))) else {
                panic!("not done")
            };
            assert_eq!(done.0.get("failed").cloned(), failed);
            assert_eq!(
                done.0["msg"],
                json!("the remote file does not exist, not transferring, ignored")
            );
        }
        assert!(!dir.join("fetched").exists());

        let mut plugin = start_plain(json!({"src": "/etc", "dest": dest}));
        plugin.next(None);
        let Step::Done(done) = plugin.next(Some(result(
            json!({"stat": {"exists": true, "isdir": true}}),
        ))) else {
            panic!("not done")
        };
        assert_eq!(done.0["msg"], json!(IS_A_DIRECTORY));

        // Under `become` it is `slurp` that says so, and an answer the agent refused as too big
        // comes back in its own words.
        for (msg, want) in [
            ("source is a directory and must be a file", IS_A_DIRECTORY),
            (
                "the answer exceeds 4194304 bytes",
                "the answer exceeds 4194304 bytes",
            ),
        ] {
            let mut plugin = start_with(
                json!({"src": "/etc", "dest": dest}),
                &BTreeSet::new(),
                "probe-hostname",
                true,
            );
            plugin.next(None);
            let Step::Done(done) = plugin.next(Some(result(json!({"failed": true, "msg": msg}))))
            else {
                panic!("not done")
            };
            assert_eq!(done.0["failed"], json!(true));
            assert_eq!(done.0["msg"], json!(want));
        }
    }

    /// A file whose bytes do not match the sum `stat` reported is not written, and the result
    /// names both sums; with `validate_checksum: false` it is written.
    ///
    /// What would make this red: the check dropped, which reports `changed` for bytes that are not
    /// the host's file, or the mismatched file renamed into place.
    #[test]
    fn a_checksum_mismatch_writes_nothing() {
        let dir = scratch("mismatch");
        let dest = dir.join("fetched");
        let file = dest.join("probe-hostname/one.txt");
        for validate in [true, false] {
            let mut plugin = start_plain(json!({"src": "/one.txt",
                "dest": dest.display().to_string(), "validate_checksum": validate}));
            plugin.next(None);
            let stated = json!({"stat": {"exists": true, "isdir": false, "path": "/one.txt",
                "checksum": sha1_hex(b"before")}});
            plugin.next(Some(result(stated)));
            let Step::Done(done) = plugin
                .next(Some(result(json!({"content": b64_encode(b"after"),
                "source": "/one.txt", "encoding": "base64"}))))
            else {
                panic!("not done")
            };
            assert_eq!(done.0["checksum"], json!(sha1_hex(b"after")));
            assert_eq!(done.0["remote_checksum"], json!(sha1_hex(b"before")));
            if validate {
                assert_eq!(done.0["msg"], json!("checksum mismatch"));
                assert_eq!(done.0["failed"], json!(true));
                assert!(!file.exists(), "the mismatched file was written");
            } else {
                assert_eq!(done.0["changed"], json!(true));
                assert_eq!(std::fs::read(&file).unwrap(), b"after");
            }
        }
    }

    /// A write that fails leaves no temporary file behind.
    ///
    /// What would make this red: the temporary file left when the rename fails, here because the
    /// place is a directory, the same path a full disk takes.
    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let dir = scratch("failed-write");
        let dest = dir.join("fetched");
        let file = dest.join("probe-hostname/one.txt");
        std::fs::create_dir_all(&file).unwrap();
        let mut plugin =
            start_plain(json!({"src": "/one.txt", "dest": dest.display().to_string()}));
        let (_, result) = drive(plugin.as_mut(), b"x", "/one.txt");
        assert_eq!(result["failed"], json!(true), "{result}");
        assert!(
            result["msg"]
                .as_str()
                .unwrap()
                .starts_with("Failed to fetch the file: "),
            "{result}"
        );
        assert!(!dest.join("probe-hostname/one.txt.volant-tmp").exists());
    }

    /// What is refused before any sub-task, in the reference's words.
    ///
    /// What would make this red: a check dropped, which sends `stat` for a task with no `dest`.
    #[test]
    fn missing_or_mistyped_arguments_are_refused() {
        for (args, msg) in [
            (json!({"dest": "/tmp/x"}), "src and dest are required"),
            (json!({"src": "/x"}), "src and dest are required"),
            (
                json!({"src": 1, "dest": "/tmp/x"}),
                "Invalid type supplied for source option, it must be a string",
            ),
            (
                json!({"src": "/x", "dest": ["/tmp"]}),
                "Invalid type supplied for dest option, it must be a string",
            ),
        ] {
            let mut plugin = start_plain(args.clone());
            let Step::Done(done) = plugin.next(None) else {
                panic!("a sub-task was sent for {args}")
            };
            assert_eq!(done.0["msg"], json!(msg), "{args}");
        }
    }

    /// `.` goes, `..` takes the component before it and stops at the root, and nothing is looked
    /// up.
    #[test]
    fn a_path_is_normalised_without_the_file_system() {
        assert_eq!(normalise(Path::new("/a/./b//../c")), Path::new("/a/c"));
        assert_eq!(normalise(Path::new("/a/../../../x")), Path::new("/x"));
        assert_eq!(normalise(Path::new("a/../../x")), Path::new("../x"));
        let pb = Path::new("/pb");
        assert_eq!(absolute("out", pb), Path::new("/pb/out"));
        assert_eq!(absolute("/abs", pb), Path::new("/abs"));
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
        assert_eq!(absolute("~/.kube/config", pb), home.join(".kube/config"));
        assert_eq!(absolute("~", pb), home);
    }
}
