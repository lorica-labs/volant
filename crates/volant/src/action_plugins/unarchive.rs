// SPDX-License-Identifier: GPL-3.0-or-later
//! `unarchive`: an archive read on the controller, or already on the host, extracted by the
//! `unarchive` module.
//!
//! Read off `plugins/action/unarchive.py` of ansible-core 2.19.12 and measured against it:
//! `copy:` is translated to `remote_src` (the two together refused); `src` and `dest` are
//! required; a `creates` that already exists on the host skips the task before anything else is
//! touched; the destination must exist and be a directory; the source is searched, refused if a
//! host named it, and read, unless `remote_src`; the module then runs with the task's arguments,
//! `decrypt` removed, and the transferred source in `src`.
//!
//! `creates` and `dest` are host paths the reference expands with a leading `~` before it asks
//! the host anything. Volant refuses one by name instead of expanding it: no measured role writes
//! one, and expanding it correctly needs the host's effective user, a fact this dispatch does not
//! have at hand. `creates`'s existence, a shell test in the reference, is asked with a `stat`
//! here, since a sub-task in this dispatch is always a module of the run's union: a `stat` that
//! fails is read the way that shell test would be, as "not there", never as a task failure.

use std::collections::BTreeSet;

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use super::copy::{failing, stat, stat_of};
use super::files::{blob_of, fits, not_found, refuse_host_named, search_paths};
use super::{Context, Plugin, Step, Sub, lost};
use crate::compile::Origin;
use crate::executor::as_bool_value;

fn flag(args: &Map<String, Value>, name: &str, default: bool) -> bool {
    args.get(name).and_then(as_bool_value).unwrap_or(default)
}

fn text<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

pub(super) fn start(ctx: Context<'_>) -> Box<dyn Plugin> {
    match Unarchive::new(&ctx) {
        Ok(plugin) => Box::new(plugin),
        Err(msg) => failing(msg),
    }
}

enum State {
    Start,
    Creates,
    Dest,
    Module,
}

struct Unarchive {
    /// The task's arguments, `copy` folded into `remote_src` when it was given.
    args: Map<String, Value>,
    dest: String,
    creates: Option<String>,
    remote_src: bool,
    untrusted: BTreeSet<String>,
    origin: Origin,
    playbook_dir: std::path::PathBuf,
    state: State,
}

impl Unarchive {
    fn new(ctx: &Context<'_>) -> Result<Self, String> {
        let mut args = ctx.args.clone();
        let mut remote_src = flag(&args, "remote_src", false);
        // The reference's checks, in its order and in its words.
        if let Some(copy) = args.remove("copy") {
            if args.contains_key("remote_src") {
                return Err("parameters are mutually exclusive: ('copy', 'remote_src')".into());
            }
            remote_src = !as_bool_value(&copy).unwrap_or(true);
            args.insert("remote_src".into(), Value::Bool(remote_src));
        }
        let (Some(_), Some(dest)) = (text(&args, "src"), text(&args, "dest")) else {
            return Err("src (or content) and dest are required".into());
        };
        let dest = dest.to_string();
        let creates = text(&args, "creates").map(str::to_string);
        if dest.starts_with('~') || creates.as_deref().is_some_and(|c| c.starts_with('~')) {
            return Err("a '~' path is not supported yet on 'unarchive'".into());
        }
        Ok(Unarchive {
            args,
            dest,
            creates,
            remote_src,
            untrusted: ctx.args_untrusted.clone(),
            origin: ctx.origin.clone(),
            playbook_dir: ctx.playbook_dir.to_path_buf(),
            state: State::Start,
        })
    }

    /// The source read locally and staged, or `remote_src` handed the task's `src` as it is.
    fn source_and_send(&mut self) -> Step {
        let mut args = self.args.clone();
        args.remove("decrypt");
        if self.remote_src {
            self.state = State::Module;
            return Sub::run("unarchive", args);
        }
        if let Err(msg) = refuse_host_named(&self.untrusted, "src") {
            return Step::Done(TaskResult::failed_with(msg));
        }
        let src = self.args.get("src").and_then(Value::as_str).unwrap_or("");
        let searched = search_paths(&self.origin, &self.playbook_dir, "files", src);
        let Some(found) = searched.iter().find(|p| p.exists()) else {
            return Step::Done(TaskResult::failed_with(format!(
                "Task failed: {}\nIf you are using a module and expect the file to exist on the remote, see the remote_src option",
                not_found(src, &searched)
            )));
        };
        // Named, as `copy` names it, rather than left to the read's own `Is a directory`.
        if found.is_dir() {
            return Step::Done(TaskResult::failed_with(format!(
                "src is a directory, not an archive: {src}"
            )));
        }
        let basename = found
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let unreadable =
            |err: std::io::Error| format!("could not read src={}: {err}", found.display());
        let len = match std::fs::metadata(found) {
            Ok(meta) => meta.len(),
            Err(err) => return Step::Done(TaskResult::failed_with(unreadable(err))),
        };
        if let Err(msg) = fits(&basename, usize::try_from(len).unwrap_or(usize::MAX)) {
            return Step::Done(TaskResult::failed_with(msg));
        }
        let bytes = match std::fs::read(found) {
            Ok(bytes) => bytes,
            Err(err) => return Step::Done(TaskResult::failed_with(unreadable(err))),
        };
        let blob = match blob_of(&basename, &bytes) {
            Ok(blob) => blob,
            Err(msg) => return Step::Done(TaskResult::failed_with(msg)),
        };
        args.remove("src");
        self.state = State::Module;
        Step::Run(Sub {
            module: "unarchive",
            args,
            files: vec![("src".into(), blob)],
        })
    }
}

/// `{"changed": false, "msg": ..., "skipped": true}`, measured: the reference's
/// `AnsibleActionSkip` registers exactly these three keys.
fn skip(msg: String) -> TaskResult {
    let mut map = Map::new();
    map.insert("changed".into(), Value::Bool(false));
    map.insert("msg".into(), Value::String(msg));
    map.insert("skipped".into(), Value::Bool(true));
    TaskResult(map)
}

impl Plugin for Unarchive {
    fn next(&mut self, last: Option<TaskResult>) -> Step {
        match self.state {
            State::Start => {
                if let Some(creates) = self.creates.clone() {
                    self.state = State::Creates;
                    // Existence only: no sum, no size, the way a shell "does this exist" would
                    // ask for none either.
                    stat(&creates, true, false)
                } else {
                    self.state = State::Dest;
                    stat(&self.dest, true, true)
                }
            }
            State::Creates => {
                let Some(result) = last else {
                    return Step::Done(lost("stat"));
                };
                let exists = !result.failed()
                    && result
                        .0
                        .get("stat")
                        .and_then(|s| s.get("exists"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                if exists {
                    let creates = self.creates.clone().unwrap_or_default();
                    return Step::Done(skip(format!("skipped, since {creates} exists")));
                }
                self.state = State::Dest;
                stat(&self.dest, true, true)
            }
            State::Dest => {
                let Some(result) = last else {
                    return Step::Done(lost("stat"));
                };
                match stat_of(&self.dest, result) {
                    Ok(found) => {
                        let is =
                            |key: &str| found.get(key).and_then(Value::as_bool).unwrap_or(false);
                        if !is("exists") || !is("isdir") {
                            return Step::Done(TaskResult::failed_with(format!(
                                "dest '{}' must be an existing dir",
                                self.dest
                            )));
                        }
                        self.source_and_send()
                    }
                    Err(failure) => Step::Done(failure),
                }
            }
            State::Module => Step::Done(last.unwrap_or_else(|| lost("unarchive"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::*;
    use crate::action_plugins::files::MAX_FILE_LEN;

    /// A directory of this test process's own under the system's temporary directory, holding
    /// `files/` and an existing `dest/`.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("volant-unarchive-{}-{name}", std::process::id()));
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::create_dir_all(dir.join("dest")).unwrap();
        dir
    }

    fn start_in(
        args: &Map<String, Value>,
        untrusted: &BTreeSet<String>,
        dir: &Path,
    ) -> Box<dyn Plugin> {
        let running = crate::vars::HostVars::default();
        let item_vars = crate::vars::HostVars::default();
        let templar = crate::template::Templar::new(dir.to_path_buf());
        let origin = Origin {
            file_dir: dir.to_path_buf(),
            ..Origin::default()
        };
        let mut warnings = Vec::new();
        start(Context {
            args,
            args_untrusted: untrusted,
            running_vars: &running,
            delegated: false,
            escalated: false,
            local: false,
            item_vars: &item_vars,
            templar: &templar,
            origin: &origin,
            playbook_dir: dir,
            warnings: &mut warnings,
        })
    }

    fn start_plain(args: &Map<String, Value>, dir: &Path) -> Box<dyn Plugin> {
        start_in(args, &BTreeSet::new(), dir)
    }

    fn run(plugin: &mut dyn Plugin, last: Option<Value>) -> Sub {
        let last = last.map(|v| match v {
            Value::Object(map) => TaskResult(map),
            _ => unreachable!(),
        });
        match plugin.next(last) {
            Step::Run(sub) => sub,
            other => panic!("done early: {other:?}"),
        }
    }

    fn done(plugin: &mut dyn Plugin, last: Value) -> Value {
        let Value::Object(last) = last else {
            unreachable!()
        };
        match plugin.next(Some(TaskResult(last))) {
            Step::Done(result) => Value::Object(result.0),
            other => panic!("another sub-task: {other:?}"),
        }
    }

    fn refused(plugin: &mut dyn Plugin) -> String {
        match plugin.next(None) {
            Step::Done(result) => {
                assert!(result.failed(), "{result:?}");
                result.0["msg"].as_str().unwrap().to_string()
            }
            other => panic!("a sub-task was sent: {other:?}"),
        }
    }

    /// Arguments unpacking an archive found under `files/` into the scratch `dest/`.
    fn args(dir: &Path, extra: Value) -> Map<String, Value> {
        std::fs::write(dir.join("files/bundle.tar.gz"), b"not-really-a-tarball").unwrap();
        let Value::Object(mut args) = json!({
            "src": "bundle.tar.gz",
            "dest": dir.join("dest").display().to_string(),
        }) else {
            unreachable!()
        };
        if let Value::Object(extra) = extra {
            args.extend(extra);
        }
        args
    }

    /// A destination confirmed to exist and be a directory: what every case beyond the first two
    /// sub-tasks needs to reach the source.
    fn past_dest(plugin: &mut dyn Plugin, has_creates: bool) {
        if has_creates {
            let sub = run(plugin, None);
            assert_eq!(sub.module, "stat");
            run(plugin, Some(json!({"stat": {"exists": false}})));
        } else {
            run(plugin, None);
        }
    }

    /// A `src` whose render read a managed host is never read, even once the destination is
    /// confirmed to be a real directory.
    ///
    /// Measured on ansible-core 2.19.12: an `unarchive` whose `src` is a registered `stdout`
    /// sends the controller file it names to the host. This refuses it, on purpose, the same way
    /// `copy` and `template` do.
    ///
    /// What would make this red: the refusal skipped or checked before the destination is known
    /// to be a directory, which would mask it behind "dest must be an existing dir" for the
    /// second path, which names nothing on the controller.
    #[test]
    fn a_source_a_host_named_is_never_read() {
        let dir = scratch("secret");
        let secret = dir.join("controller-secret");
        std::fs::write(&secret, "secret").unwrap();
        let untrusted: BTreeSet<String> = ["src".to_string()].into();
        for named in [secret.clone(), dir.join("no-such-file")] {
            let mut task_args = Map::new();
            task_args.insert("src".into(), json!(named.display().to_string()));
            task_args.insert("dest".into(), json!(dir.join("dest").display().to_string()));
            let mut plugin = start_in(&task_args, &untrusted, &dir);
            run(plugin.as_mut(), None);
            let result = done(
                plugin.as_mut(),
                json!({"stat": {"exists": true, "isdir": true}}),
            );
            assert_eq!(
                result["msg"],
                json!(
                    "the 'src' of this task was named by a managed host, and a controller file a host chose is never sent"
                ),
                "{}",
                named.display()
            );
        }
    }

    /// `copy:` folds into `remote_src`, and the two together are refused before any host is
    /// asked anything.
    ///
    /// What would make this red: `copy` left in the module's own arguments, which the module
    /// does not accept, or the exclusivity check dropped, silently preferring one of the two.
    #[test]
    fn copy_becomes_remote_src_and_the_two_together_are_refused() {
        let dir = scratch("copy-flag");
        let mut task_args = args(&dir, json!({"copy": false}));
        let mut plugin = start_plain(&task_args, &dir);
        run(plugin.as_mut(), None);
        let sub = run(
            plugin.as_mut(),
            Some(json!({"stat": {"exists": true, "isdir": true}})),
        );
        assert_eq!(sub.args["remote_src"], json!(true));
        assert!(!sub.args.contains_key("copy"), "{:?}", sub.args);

        task_args.insert("remote_src".into(), json!(false));
        let mut plugin = start_plain(&task_args, &dir);
        assert_eq!(
            refused(plugin.as_mut()),
            "parameters are mutually exclusive: ('copy', 'remote_src')"
        );
    }

    /// What is refused before any sub-task, each in the reference's words.
    ///
    /// What would make this red: a check dropped, which runs `stat` for a task that cannot
    /// succeed, or a leading `~` read as an ordinary character and searched for as one.
    #[test]
    fn what_cannot_be_unpacked_is_refused_before_the_host_is_asked() {
        let dir = scratch("refused");
        for (extra, msg) in [
            (
                json!({"dest": null}),
                "src (or content) and dest are required",
            ),
            (
                json!({"src": null}),
                "src (or content) and dest are required",
            ),
            (
                json!({"dest": "~/there"}),
                "a '~' path is not supported yet on 'unarchive'",
            ),
            (
                json!({"creates": "~/there"}),
                "a '~' path is not supported yet on 'unarchive'",
            ),
        ] {
            let mut task_args = args(&dir, json!({}));
            if let Value::Object(extra) = extra {
                for (k, v) in extra {
                    if v.is_null() {
                        task_args.remove(&k);
                    } else {
                        task_args.insert(k, v);
                    }
                }
            }
            let mut plugin = start_plain(&task_args, &dir);
            assert_eq!(refused(plugin.as_mut()), msg, "{task_args:?}");
        }
    }

    /// `creates` already there on the host skips the task before `dest` is even looked at, and
    /// nothing is sent.
    ///
    /// Measured on ansible-core 2.19.12: `{"changed": false, "msg": "skipped, since <path>
    /// exists", "skipped": true}`.
    ///
    /// What would make this red: the sum or the size asked for in the `creates` `stat`, which a
    /// shell existence test never computes, or the skip decided on a failed `stat` instead of
    /// read as "not there".
    #[test]
    fn creates_already_there_skips_before_dest_is_asked() {
        let dir = scratch("creates");
        let task_args = args(&dir, json!({"creates": "/tmp/unarchive-marker"}));
        let mut plugin = start_plain(&task_args, &dir);
        let sub = run(plugin.as_mut(), None);
        assert_eq!(sub.module, "stat");
        assert_eq!(
            Value::Object(sub.args),
            json!({"path": "/tmp/unarchive-marker", "follow": true, "get_checksum": false,
                   "checksum_algorithm": "sha1"})
        );
        let result = done(
            plugin.as_mut(),
            json!({"stat": {"exists": true, "isdir": false}}),
        );
        assert_eq!(
            result,
            json!({"changed": false, "msg": "skipped, since /tmp/unarchive-marker exists",
                   "skipped": true})
        );

        // A `stat` that fails is read the way the reference's shell test reads a command that
        // could not run: as "not there", not as a task failure.
        let mut plugin = start_plain(&task_args, &dir);
        run(plugin.as_mut(), None);
        let sub = run(
            plugin.as_mut(),
            Some(json!({"failed": true, "msg": "boom"})),
        );
        assert_eq!(sub.module, "stat", "creates absent, dest is asked next");
        assert_eq!(
            sub.args["path"],
            json!(dir.join("dest").display().to_string())
        );
    }

    /// The destination must exist and be a directory, `stat`'s own words when it fails, and the
    /// reference's sentence otherwise.
    ///
    /// What would make this red: a file dest accepted, which hands the module a path it cannot
    /// extract into, or a failed `stat` read as a missing destination without the reference's
    /// wording.
    #[test]
    fn a_destination_that_is_not_a_directory_is_refused() {
        let dir = scratch("dest-file");
        let task_args = args(&dir, json!({}));
        for stat_result in [
            json!({"stat": {"exists": false}}),
            json!({"stat": {"exists": true, "isdir": false}}),
        ] {
            let mut plugin = start_plain(&task_args, &dir);
            run(plugin.as_mut(), None);
            let result = done(plugin.as_mut(), stat_result);
            assert_eq!(
                result["msg"],
                json!(format!(
                    "dest '{}' must be an existing dir",
                    dir.join("dest").display()
                ))
            );
        }
        let mut plugin = start_plain(&task_args, &dir);
        run(plugin.as_mut(), None);
        let result = done(
            plugin.as_mut(),
            json!({"failed": true, "msg": "Permission denied"}),
        );
        assert_eq!(
            result["msg"],
            json!(format!(
                "Failed to get information on remote file ({}): Permission denied",
                dir.join("dest").display()
            ))
        );
    }

    /// A local source is searched, refused if a host named it, staged, and the module runs with
    /// `decrypt` and `src` gone from its own arguments, the staged bytes in `files`.
    ///
    /// What would make this red: `decrypt` reaching the module, which it does not accept, or the
    /// controller's path left in `src`, which the host would read as its own.
    #[test]
    fn a_local_source_is_staged_and_sent() {
        let dir = scratch("local");
        let task_args = args(&dir, json!({"decrypt": false}));
        let mut plugin = start_plain(&task_args, &dir);
        past_dest(plugin.as_mut(), false);
        let sub = run(
            plugin.as_mut(),
            Some(json!({"stat": {"exists": true, "isdir": true}})),
        );
        assert_eq!(sub.module, "unarchive");
        assert_eq!(
            Value::Object(sub.args),
            json!({"dest": dir.join("dest").display().to_string()})
        );
        assert_eq!(
            sub.files,
            [(
                "src".to_string(),
                blob_of("bundle.tar.gz", b"not-really-a-tarball").unwrap()
            )]
        );
        let result = done(
            plugin.as_mut(),
            json!({"changed": true, "dest": dir.join("dest").display().to_string(), "state": "directory"}),
        );
        assert_eq!(
            result,
            json!({"changed": true, "dest": dir.join("dest").display().to_string(),
                   "state": "directory"}),
            "the module's result travels as it is, no diff added"
        );
    }

    /// `remote_src` runs the module alone, with the task's own `src`, and nothing is searched,
    /// read or staged.
    ///
    /// Measured on ansible-core 2.19.12: no transfer, `unarchive` alone with the arguments as
    /// given less `decrypt`.
    #[test]
    fn remote_src_sends_the_module_alone() {
        let dir = scratch("remote");
        let mut task_args = args(&dir, json!({"remote_src": true, "decrypt": false}));
        task_args.insert("src".into(), json!("/on/the/host.tar.gz"));
        let mut plugin = start_in(&task_args, &["src".to_string()].into(), &dir);
        run(plugin.as_mut(), None);
        let sub = run(
            plugin.as_mut(),
            Some(json!({"stat": {"exists": true, "isdir": true}})),
        );
        assert_eq!(sub.module, "unarchive");
        assert_eq!(
            Value::Object(sub.args),
            json!({"src": "/on/the/host.tar.gz", "dest": dir.join("dest").display().to_string(),
                   "remote_src": true})
        );
        assert!(sub.files.is_empty(), "nothing is staged: {:?}", sub.files);
    }

    /// A source too big for one frame is refused by its size, before it is read.
    ///
    /// What would make this red: the size checked only on the bytes once read, which loads the
    /// whole archive and stages it for a transfer that can never happen.
    #[test]
    fn a_source_bigger_than_a_frame_is_refused_before_it_is_read() {
        let dir = scratch("big");
        let mut task_args = args(&dir, json!({}));
        let big = std::fs::File::create(dir.join("files/big.tar.gz")).unwrap();
        let len = MAX_FILE_LEN + 1;
        big.set_len(u64::try_from(len).unwrap()).unwrap();
        task_args.insert("src".into(), json!("big.tar.gz"));
        let mut plugin = start_plain(&task_args, &dir);
        run(plugin.as_mut(), None);
        let result = done(
            plugin.as_mut(),
            json!({"stat": {"exists": true, "isdir": true}}),
        );
        assert_eq!(
            result["msg"],
            json!(format!(
                "big.tar.gz is {len} bytes; one frame carries at most {MAX_FILE_LEN}"
            ))
        );
    }

    /// A `src` that is a directory on the controller is refused by name once `dest` is confirmed,
    /// and nothing is staged.
    ///
    /// What would make this red: the directory handed to the read, which fails with the
    /// platform's `Is a directory` wording and no hint of what was wrong with the task.
    #[test]
    fn a_directory_source_is_refused_by_name() {
        let dir = scratch("dir-src");
        let mut task_args = args(&dir, json!({}));
        std::fs::create_dir_all(dir.join("files/tree")).unwrap();
        task_args.insert("src".into(), json!("tree"));
        let mut plugin = start_plain(&task_args, &dir);
        run(plugin.as_mut(), None);
        let result = done(
            plugin.as_mut(),
            json!({"stat": {"exists": true, "isdir": true}}),
        );
        assert_eq!(
            result["msg"],
            json!("src is a directory, not an archive: tree")
        );
    }

    /// A `src` that names nothing on the controller is refused with the reference's own wording,
    /// which points the operator at `remote_src`.
    #[test]
    fn a_missing_source_names_where_it_looked() {
        let dir = scratch("missing");
        let mut task_args = args(&dir, json!({}));
        task_args.insert("src".into(), json!("nope.tar.gz"));
        let mut plugin = start_plain(&task_args, &dir);
        run(plugin.as_mut(), None);
        let result = done(
            plugin.as_mut(),
            json!({"stat": {"exists": true, "isdir": true}}),
        );
        let searched = search_paths(
            &Origin {
                file_dir: dir.clone(),
                ..Origin::default()
            },
            &dir,
            "files",
            "nope.tar.gz",
        );
        assert_eq!(
            result["msg"],
            json!(format!(
                "Task failed: {}\nIf you are using a module and expect the file to exist on the remote, see the remote_src option",
                not_found("nope.tar.gz", &searched)
            ))
        );
    }
}
