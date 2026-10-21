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
//! The path written is built, normalised without following anything, and refused unless it sits
//! under `dest` (and under `dest/<inventory_hostname>/` without `flat`). This is stricter than
//! the reference. Measured on ansible-core 2.19.12, the reference checks the containment before
//! it builds the path, so a relative `src` of `../../../../../../../../tmp/x` without `flat`
//! writes `/tmp/x` on the controller. A symbolic link below `dest` is refused as well, so a link
//! planted inside it cannot send the write elsewhere.
//!
//! A `dest` whose render read a managed host is written as the reference writes it, unless the
//! operator asked for the protection: `host_named_dest_dirs` is set, or `host_named_paths` is
//! `contained` or `refuse`. The `dest` must then resolve strictly inside one of those
//! directories (see `dest`).

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::encoding::{b64_decode, sha1_hex};

use super::copy::{failing, stat, stat_of};
use super::{Context, Plugin, Step, Sub, lost};
use crate::executor::as_bool_value;

mod dest;
mod paths;

use paths::{absolute, normalise, walk};

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
    /// The directories `dest` must stay inside when a managed host named it.
    within: Option<Vec<PathBuf>>,
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
        let text = |name| {
            arg(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (src, dest) = (text("src"), text("dest"));
        let limit = dest::limit(ctx.host_named_paths, ctx.host_named_dest_dirs);
        let within = match limit {
            Some(dirs) if ctx.args_untrusted.contains("dest") => {
                dest::check_text(&dest, dirs)?;
                Some(dirs.to_vec())
            }
            _ => None,
        };
        let flat = flag(args, "flat", false);
        let host = ctx
            .item_vars
            .get("inventory_hostname")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // A loop variable named `inventory_hostname` whose value came from a host would let it
        // pick which host's directory it writes into. Only the protection refuses it.
        if limit.is_some() && !flat && ctx.item_vars.untrusted.contains("inventory_hostname") {
            return Err("the 'inventory_hostname' of this item was named by a managed host, and a controller path a host chose is never written".into());
        }
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
            within,
            flat,
            fail_on_missing: flag(args, "fail_on_missing", true),
            validate_checksum: flag(args, "validate_checksum", true),
            escalated: ctx.escalated,
            remote_checksum: None,
            state: State::Start,
        };
        fetch.contained()?;
        // Once before the host is asked anything, on the path as the playbook wrote it, and again
        // on the one the host names back.
        fetch.place(&fetch.src)?;
        Ok(fetch)
    }

    /// The refusal of a `dest` a host named that resolves outside `host_named_dest_dirs`, or the
    /// directory of the list the walk for links then starts from.
    fn contained(&self) -> Result<Option<PathBuf>, String> {
        match &self.within {
            Some(dirs) => dest::check_resolved(&self.root, &self.dest, dirs).map(Some),
            None => Ok(None),
        }
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
        let anchor = self.contained()?;
        let path = self.place(source)?;
        walk(
            anchor.as_deref().unwrap_or(self.walk_root()),
            &path,
            false,
            anchor.is_some(),
        )?;
        if self.flat && path.is_dir() {
            return Err(if self.dest.ends_with('/') {
                format!(
                    "calculated dest '{}' is an existing directory, use another path that does not point to an existing directory",
                    path.display()
                )
            } else {
                "dest is an existing directory, use a trailing slash if you want to fetch src into that directory".into()
            });
        }
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
            let mut out = Map::new();
            if self.fail_on_missing {
                out.insert("changed".into(), Value::Bool(false));
                out.insert("failed".into(), Value::Bool(true));
                out.insert("msg".into(), Value::String(IS_A_DIRECTORY.into()));
            } else {
                out.insert(
                    "msg".into(),
                    Value::String(format!("{IS_A_DIRECTORY}, not transferring, ignored")),
                );
            }
            return Step::Done(TaskResult(out));
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
                "the remote file does not exist, not transferring, ignored"
            } else if msg.starts_with("source is a directory") {
                IS_A_DIRECTORY
            } else {
                // Any other failure - the agent refusing an answer too big to carry, a read the
                // host denied - fails the task in its own words, whatever `fail_on_missing` says:
                // the file is there and nothing was written.
                return result;
            };
            let msg = msg.to_string();
            // The module's own result under `fail_on_missing`, as the reference hands it back.
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
        // A name of this write's own: two hosts fetching `flat` to one path run at once, each on
        // its own driver, and a shared temporary name would mix their bytes.
        static WRITES: AtomicU64 = AtomicU64::new(0);
        let mut tmp = path.clone().into_os_string();
        tmp.push(format!(
            ".{}-{}.volant-tmp",
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        ));
        let tmp = PathBuf::from(tmp);
        // A `dest` a host named is walked from the directory of the list, so no link and no
        // directory another user controls stands between it and the file.
        let written = self.contained().and_then(|anchor| {
            walk(
                anchor.as_deref().unwrap_or(self.walk_root()),
                &path,
                true,
                anchor.is_some(),
            )
        });
        let written = written.and_then(|()| {
            let fail = |err: std::io::Error| format!("Failed to fetch the file: {err}");
            // The file being replaced keeps its mode, as the reference's write in place does: a
            // `0600` kubeconfig fetched again stays `0600`. `walk` refused a link at `path`.
            let kept = std::fs::symlink_metadata(&path)
                .ok()
                .filter(std::fs::Metadata::is_file)
                .map(|m| m.permissions());
            let mut file = std::fs::File::create_new(&tmp).map_err(fail)?;
            if let Some(kept) = kept {
                file.set_permissions(kept).map_err(fail)?;
            }
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

#[cfg(test)]
mod tests;
