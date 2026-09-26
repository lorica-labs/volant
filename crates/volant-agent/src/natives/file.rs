// SPDX-License-Identifier: GPL-3.0-or-later
//! `file`, answered in the agent as `ansible/modules/file.py` of ansible-core 2.19.12 answers,
//! for `state` `file`, `directory` and `absent` (or none, which reads the path's current state).
//!
//! The native reads and decides first, then acts in the reference's own order (`mkdir`, owner,
//! group, mode). Every hand-back comes before the first change; a failure after it is the
//! reference's own failure, with whatever was already done left done, as the reference leaves it.
//! Outside the subset (`link`, `hard`, `touch`, `recurse`, timestamps, `attributes`, SELinux, a
//! task `environment`...) the Python module answers.

use std::fs;
use std::os::unix::fs::MetadataExt;

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use super::common::{
    Account, ArgSpec, FsError, ModeError, add_path_info, bool_param, check_names, failure,
    group_account, module_args, os_error, owner_account, parse_mode, path_param, realpath,
    selinux_enabled, set_fs_attributes, str_param,
};
use super::{Native, NativeRun};
use crate::modules::Context;

pub const NATIVE: Native = Native {
    name: "file",
    aliases: &[],
    enabled: true,
    run,
};

const TIME_FORMAT: fn() -> Value = || json!("%Y%m%d%H%M.%S");

/// `file`'s own options, then `add_file_common_args`.
const SPEC: &[ArgSpec] = &[
    null("state"),
    ArgSpec {
        name: "path",
        aliases: &["dest", "name"],
        default: || Value::Null,
    },
    null("_original_basename"),
    flag("recurse", false),
    flag("force", false),
    flag("follow", true),
    null("_diff_peek"),
    null("src"),
    null("modification_time"),
    ArgSpec {
        name: "modification_time_format",
        aliases: &[],
        default: TIME_FORMAT,
    },
    null("access_time"),
    ArgSpec {
        name: "access_time_format",
        aliases: &[],
        default: TIME_FORMAT,
    },
    null("mode"),
    null("owner"),
    null("group"),
    null("seuser"),
    null("serole"),
    null("selevel"),
    null("setype"),
    ArgSpec {
        name: "attributes",
        aliases: &["attr"],
        default: || Value::Null,
    },
    flag("unsafe_writes", false),
];

const fn null(name: &'static str) -> ArgSpec {
    ArgSpec {
        name,
        aliases: &[],
        default: || Value::Null,
    }
}

const fn flag(name: &'static str, default: bool) -> ArgSpec {
    ArgSpec {
        name,
        aliases: &[],
        default: if default {
            || Value::Bool(true)
        } else {
            || Value::Bool(false)
        },
    }
}

/// Options whose every value other than `null` is outside the native.
const UNSUPPORTED: &[&str] = &[
    "_diff_peek",
    "src",
    "modification_time",
    "access_time",
    "attributes",
    "seuser",
    "serole",
    "selevel",
    "setype",
];

/// Why a state function stopped short of an answer.
enum Stop {
    /// The reference's `fail_json`, with these keys.
    Fail(Map<String, Value>),
    /// Outside the subset. Only ever returned before anything on the host changed.
    Back(String),
}

impl From<String> for Stop {
    fn from(reason: String) -> Self {
        Stop::Back(reason)
    }
}

/// The owner, group and mode a task asks for, resolved before anything changes.
struct Attrs {
    owner: Option<Account>,
    group: Option<Account>,
    mode: Option<Value>,
}

impl Attrs {
    /// Sets them on `path`. `acted` says whether this task already changed something, which
    /// turns an error the reference's module would not survive into a failure rather than a
    /// hand-back.
    fn set(
        &self,
        path: &str,
        acted: bool,
        context: impl Fn(String) -> String,
    ) -> Result<bool, Stop> {
        match set_fs_attributes(
            path,
            self.owner.as_ref(),
            self.group.as_ref(),
            self.mode.as_ref(),
        ) {
            Ok(changed) => Ok(changed),
            Err(FsError::Failed(fail)) => Err(Stop::Fail(fail)),
            Err(FsError::Raised(error, touched)) if acted || touched => {
                Err(Stop::Fail(message(context(error))))
            }
            Err(FsError::Raised(error, _)) => Err(Stop::Back(error)),
        }
    }
}

fn run(args: &Map<String, Value>, context: &Context, _: &dyn Fn() -> bool) -> NativeRun {
    match answer(args, context) {
        Ok(result) => NativeRun::Done(TaskResult(result)),
        Err(reason) => NativeRun::Fallback(reason),
    }
}

fn answer(args: &Map<String, Value>, context: &Context) -> Result<Map<String, Value>, String> {
    if !cfg!(target_os = "linux") {
        return Err("the native answers on Linux only".into());
    }
    if !context.environment.is_empty() {
        return Err("the task sets an environment".into());
    }
    if std::env::var_os("ANSIBLE_UNSAFE_WRITES").is_some() {
        return Err("ANSIBLE_UNSAFE_WRITES is set".into());
    }
    if selinux_enabled() {
        return Err("SELinux is enabled".into());
    }
    check_names(SPEC, args)?;
    let mut params = module_args(SPEC, args);
    if let Some(name) = UNSUPPORTED.iter().find(|name| !params[**name].is_null()) {
        return Err(format!("{name} is set"));
    }
    for name in ["recurse", "force", "follow", "unsafe_writes"] {
        bool_param(&params, name)?;
    }
    if bool_param(&params, "recurse")? {
        return Err("recurse is set".into());
    }
    str_param(&params, "modification_time_format")?;
    str_param(&params, "access_time_format")?;
    let follow = bool_param(&params, "follow")?;
    let mode = Some(params["mode"].clone()).filter(|mode| !mode.is_null());
    if let Some(mode) = &mode
        && let Err(ModeError::Unsupported(why)) = parse_mode(mode, 0, false)
    {
        return Err(why);
    }
    let attrs = Attrs {
        owner: str_param(&params, "owner")?
            .map(owner_account)
            .transpose()?,
        group: str_param(&params, "group")?
            .map(group_account)
            .transpose()?,
        mode,
    };
    let basename = str_param(&params, "_original_basename")?.map(str::to_string);
    let asked = str_param(&params, "state")?.map(str::to_string);
    let mut path = path_param(&params, "path")?.to_string();

    // `additional_parameter_handling`: a directory named where a file is meant stands for the
    // file of that name inside it, and no `state` means the path's current one.
    if !matches!(asked.as_deref(), Some("link" | "absent"))
        && is_dir(&path)
        && let Some(basename) = basename.filter(|name| !name.is_empty())
    {
        path = if basename.starts_with('/') {
            basename
        } else {
            format!("{}/{basename}", path.trim_end_matches('/'))
        };
        if path.contains('$') {
            return Err("the path would be expanded".into());
        }
    }
    let state = match asked {
        Some(state) => state,
        None => match get_state(&path)? {
            "absent" => "file".to_string(),
            current => current.to_string(),
        },
    };
    params.insert("path".into(), path.clone().into());
    params.insert("state".into(), state.clone().into());

    let outcome = match state.as_str() {
        "absent" => ensure_absent(&path),
        "file" => ensure_file(&path, follow, &attrs),
        "directory" => ensure_directory(&path, follow, &attrs),
        other => return Err(format!("state {other}")),
    };
    let mut result = match outcome {
        Ok(result) => result,
        Err(Stop::Fail(mut fail)) => {
            fail.insert("failed".into(), Value::Bool(true));
            fail
        }
        Err(Stop::Back(reason)) => return Err(reason),
    };
    add_path_info(&mut result);
    result.insert("invocation".into(), json!({"module_args": params}));
    Ok(result)
}

/// `get_state`: `absent`, `link`, `directory`, `hard` (a file with more than one name) or
/// `file`.
fn get_state(path: &str) -> Result<&'static str, String> {
    let Ok(lstat) = fs::symlink_metadata(path) else {
        return Ok("absent");
    };
    if lstat.file_type().is_symlink() {
        return Ok("link");
    }
    if is_dir(path) {
        return Ok("directory");
    }
    match fs::metadata(path) {
        Ok(stat) if stat.nlink() > 1 => Ok("hard"),
        Ok(_) => Ok("file"),
        Err(err) if err.raw_os_error() == Some(libc::ENOENT) => Ok("absent"),
        Err(err) => Err(format!("stat {path}: {err}")),
    }
}

fn is_dir(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_dir())
}

fn is_link(path: &str) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// `load_file_common_arguments`' path: a link's target when links are followed.
fn attrs_path(path: &str, follow: bool) -> Result<String, String> {
    if follow && is_link(path) {
        realpath(path)
    } else {
        Ok(path.to_string())
    }
}

/// `ensure_absent`.
fn ensure_absent(path: &str) -> Result<Map<String, Value>, Stop> {
    let mut result = Map::new();
    result.insert("path".into(), path.into());
    result.insert("state".into(), "absent".into());
    match get_state(path)? {
        "absent" => {
            result.insert("changed".into(), Value::Bool(false));
            return Ok(result);
        }
        "directory" => {
            // `shutil.rmtree` names the entry it failed on, which `remove_dir_all` does not say:
            // a tree with a directory the agent cannot empty goes to the Python module.
            if !removable(path) {
                return Err(Stop::Back(format!("{path} holds what cannot be removed")));
            }
            if let Err(err) = fs::remove_dir_all(path) {
                return Err(Stop::Fail(message(format!(
                    "rmtree failed: {}",
                    os_error(&err, path)
                ))));
            }
        }
        _ => match fs::remove_file(path) {
            Err(err) if err.raw_os_error() != Some(libc::ENOENT) => {
                return Err(Stop::Fail(failure(path, "Unlinking failed.".into())));
            }
            _ => {}
        },
    }
    result.insert("changed".into(), Value::Bool(true));
    Ok(result)
}

/// Whether every directory of the tree at `dir`, `dir` included, can be listed and emptied.
fn removable(dir: &str) -> bool {
    use super::common::access;
    if !access(dir, libc::R_OK | libc::W_OK | libc::X_OK) {
        return false;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.into_iter().all(|entry| {
        entry.is_ok_and(|entry| {
            !entry.file_type().is_ok_and(|kind| kind.is_dir())
                || entry.path().to_str().is_some_and(removable)
        })
    })
}

/// `ensure_file_attributes`: the path must be a file (or a link to one, when followed).
fn ensure_file(path: &str, follow: bool, attrs: &Attrs) -> Result<Map<String, Value>, Stop> {
    let mut path = path.to_string();
    let mut prev = get_state(&path)?;
    let mut target = attrs_path(&path, follow)?;
    if follow && prev == "link" {
        path = realpath(&path)?;
        prev = get_state(&path)?;
        target.clone_from(&path);
    }
    if prev != "file" && prev != "hard" {
        let mut fail = failure(&path, format!("file ({path}) is {prev}, cannot continue"));
        fail.insert("state".into(), prev.into());
        return Err(Stop::Fail(fail));
    }
    let changed = attrs.set(&target, false, |error| error)?;
    Ok(done(&path, changed))
}

/// `ensure_directory`: `mkdir -p`, each directory it creates given the owner, group and mode,
/// or those set on the directory already there.
fn ensure_directory(path: &str, follow: bool, attrs: &Attrs) -> Result<Map<String, Value>, Stop> {
    let mut path = path.to_string();
    let mut prev = get_state(&path)?;
    let mut target = attrs_path(&path, follow)?;
    if follow && prev == "link" {
        path = realpath(&path)?;
        target.clone_from(&path);
        prev = get_state(&path)?;
    }
    match prev {
        "absent" => {
            let mut changed = false;
            let mut current = String::new();
            for name in path.trim_matches('/').split('/') {
                current = format!("{current}/{name}");
                if fs::metadata(&current).is_ok() {
                    continue;
                }
                let issue = |error: String| {
                    format!("There was an issue creating {current} as requested: {error}")
                };
                match fs::create_dir(&current) {
                    Ok(()) => changed = true,
                    Err(err) if err.raw_os_error() == Some(libc::EEXIST) && is_dir(&current) => {}
                    Err(err) if changed => {
                        return Err(Stop::Fail(failure(&path, issue(os_error(&err, &current)))));
                    }
                    Err(err) => return Err(Stop::Back(os_error(&err, &current))),
                }
                changed |= attrs
                    .set(&current, changed, issue)
                    .map_err(|stop| with_path(stop, &path))?;
            }
            Ok(done(&path, changed))
        }
        "directory" => {
            let changed = attrs.set(&target, false, |error| error)?;
            Ok(done(&path, changed))
        }
        other => Err(Stop::Fail(failure(
            &path,
            format!("{path} already exists as a {other}"),
        ))),
    }
}

/// An exception inside the `mkdir -p` loop is caught there and reported with `path=path`; a
/// `fail_json` from the attributes is not caught and keeps its own.
fn with_path(stop: Stop, path: &str) -> Stop {
    match stop {
        Stop::Fail(fail) if !fail.contains_key("path") => {
            let msg = fail["msg"].as_str().unwrap_or_default().to_string();
            Stop::Fail(failure(path, msg))
        }
        other => other,
    }
}

/// A failure with only a message, and so no path information added.
fn message(msg: String) -> Map<String, Value> {
    let mut fail = Map::new();
    fail.insert("msg".into(), msg.into());
    fail
}

fn done(path: &str, changed: bool) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert("path".into(), path.into());
    result.insert("changed".into(), Value::Bool(changed));
    result
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::{Value, json};

    use super::super::common::golden::{Scratch, differences};
    use super::*;

    fn ask(args: &Value) -> NativeRun {
        run(args.as_object().unwrap(), &Context::default(), &|| false)
    }

    fn chmod(path: &str, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mkdir(path: &str, mode: u32) {
        fs::create_dir_all(path).unwrap();
        chmod(path, mode);
    }

    /// What the golden test reads back after a case: `{exists, mode, type, content}`.
    fn after(path: &str) -> Value {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return json!({"exists": false});
        };
        let kind = if meta.file_type().is_symlink() {
            "link"
        } else if meta.is_dir() {
            "directory"
        } else {
            "file"
        };
        let mode = format!("{:04o}", meta.mode() & 0o7777);
        let mut after = json!({"exists": true, "mode": mode, "type": kind});
        if kind == "file" {
            after["content"] = fs::read_to_string(path).unwrap().into();
        }
        after
    }

    /// Every recorded `file` case the native answers, each on the state the golden play left
    /// before it: removal of a missing path and of a directory, a directory created, left alone,
    /// or given a mode in octal text, as a YAML integer and in symbolic form, `dest` for `path`,
    /// a mode on a file, and the reference's four failures (a file where a directory is asked,
    /// `state=file` on nothing, an owner nobody has, a `chown` refused), with what each leaves on
    /// disk.
    ///
    /// What would make this red: `add_path_info` without `mode` (every case with a path), the
    /// resolved `state` missing from `invocation` (`file-chown-denied` has none in its
    /// arguments), a mode applied before the `chown` that fails, the symbolic `o=` leaving the
    /// other bits, `changed` for a mode already there, or a failure's `path` and `state` keys
    /// missing.
    #[test]
    fn file_answers_every_recorded_case_like_the_reference() {
        assert_ne!(
            unsafe { libc::geteuid() },
            0,
            "file-chown-denied needs a chown to be refused, and root is refused nothing"
        );
        type Setup = fn(&Scratch);
        let cases: [(&str, &str, Value, Setup); 12] = [
            (
                "file-absent-missing",
                include_str!("../../../volant/tests/golden/native/file-absent-missing.json"),
                json!({"path": "<golden-tmp>/gone", "state": "absent"}),
                |_| {},
            ),
            (
                "file-absent-present",
                include_str!("../../../volant/tests/golden/native/file-absent-present.json"),
                json!({"path": "<golden-tmp>/d0", "state": "absent"}),
                |s| {
                    mkdir(&s.path("d0/inner"), 0o755);
                    fs::write(s.path("d0/inner/x"), "x").unwrap();
                },
            ),
            (
                "file-chown-denied",
                include_str!("../../../volant/tests/golden/native/file-chown-denied.json"),
                json!({"owner": "root", "path": "<golden-tmp>/f.txt"}),
                |s| chmod(&s.path("f.txt"), 0o600),
            ),
            (
                "file-dest-alias",
                include_str!("../../../volant/tests/golden/native/file-dest-alias.json"),
                json!({"dest": "<golden-tmp>/a/b", "state": "directory"}),
                |s| mkdir(&s.path("a/b"), 0o750),
            ),
            (
                "file-dir-created",
                include_str!("../../../volant/tests/golden/native/file-dir-created.json"),
                json!({"mode": "0755", "path": "<golden-tmp>/a/b", "state": "directory"}),
                |_| {},
            ),
            (
                "file-dir-over-file",
                include_str!("../../../volant/tests/golden/native/file-dir-over-file.json"),
                json!({"path": "<golden-tmp>/f.txt", "state": "directory"}),
                |s| chmod(&s.path("f.txt"), 0o600),
            ),
            (
                "file-dir-same",
                include_str!("../../../volant/tests/golden/native/file-dir-same.json"),
                json!({"mode": 493, "path": "<golden-tmp>/a/b", "state": "directory"}),
                |s| mkdir(&s.path("a/b"), 0o755),
            ),
            (
                "file-dir-symbolic",
                include_str!("../../../volant/tests/golden/native/file-dir-symbolic.json"),
                json!({"mode": "u=rwx,g=rx,o=", "path": "<golden-tmp>/a/b", "state": "directory"}),
                |s| mkdir(&s.path("a/b"), 0o755),
            ),
            (
                "file-owner-unknown",
                include_str!("../../../volant/tests/golden/native/file-owner-unknown.json"),
                json!({"owner": "volant-no-such-user", "path": "<golden-tmp>/f.txt"}),
                |s| chmod(&s.path("f.txt"), 0o600),
            ),
            (
                "file-state-file-missing",
                include_str!("../../../volant/tests/golden/native/file-state-file-missing.json"),
                json!({"path": "<golden-tmp>/missing", "state": "file"}),
                |_| {},
            ),
            (
                "file-state-file-mode",
                include_str!("../../../volant/tests/golden/native/file-state-file-mode.json"),
                json!({"mode": "0600", "path": "<golden-tmp>/f.txt", "state": "file"}),
                |_| {},
            ),
            (
                "file-state-file-same",
                include_str!("../../../volant/tests/golden/native/file-state-file-same.json"),
                json!({"path": "<golden-tmp>/f.txt", "state": "file"}),
                |_| {},
            ),
        ];
        let mut found = Vec::new();
        for (case, recording, args, setup) in cases {
            let scratch = Scratch::new(case);
            scratch.fixture();
            setup(&scratch);
            let args: Value =
                serde_json::from_str(&args.to_string().replace("<golden-tmp>", &scratch.0))
                    .unwrap();
            let path = args["path"].as_str().or(args["dest"].as_str()).unwrap();
            match ask(&args) {
                NativeRun::Done(result) => found.extend(
                    differences(recording, result.0, &scratch, &[])
                        .into_iter()
                        .map(|d| format!("{case}: {d}")),
                ),
                NativeRun::Fallback(why) => found.push(format!("{case}: handed back, {why}")),
                NativeRun::Cancelled => found.push(format!("{case}: cancelled")),
            }
            let want: Value = serde_json::from_str(recording).unwrap();
            if after(path) != want["_after"] {
                found.push(format!(
                    "{case}: left {}, the reference {}",
                    after(path),
                    want["_after"]
                ));
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }

    /// Outside the subset the task goes to the Python module, and the disk is as it was: a link
    /// (`file-link`), `recurse` (`file-recurse`), a state the reference infers as `link`,
    /// timestamps, `attributes`, a task `environment`, a mode Python reads as an integer
    /// (`0o600`), `touch`, a boolean the reference would convert.
    ///
    /// What would make this red: any of those answered by the native, or anything created or
    /// changed before handing back.
    #[test]
    fn file_hands_back_before_changing_anything() {
        let scratch = Scratch::new("file-back");
        let file = scratch.fixture();
        mkdir(&scratch.path("a/b"), 0o700);
        let dir = scratch.path("a");
        let link = scratch.path("l2");
        let before = || {
            [
                after(&file),
                after(&dir),
                after(&scratch.path("a/b")),
                after(&link),
            ]
        };
        let snapshot = before();
        let mut environment = Context::default();
        environment
            .environment
            .insert("PATH".into(), "/nowhere".into());
        let plain = Context::default;
        for (args, context) in [
            (json!({"path": link, "src": file, "state": "link"}), plain()),
            (
                json!({"path": dir, "mode": "0755", "recurse": true, "state": "directory"}),
                plain(),
            ),
            (json!({"path": scratch.path("l"), "mode": "0600"}), plain()),
            (json!({"path": file, "modification_time": "now"}), plain()),
            (json!({"path": file, "attributes": "+i"}), plain()),
            (json!({"path": file, "mode": "0600"}), environment),
            (json!({"path": file, "mode": "0o600"}), plain()),
            (json!({"path": file, "state": "touch"}), plain()),
            (
                json!({"path": dir, "mode": "0755", "state": "directory", "follow": "no"}),
                plain(),
            ),
        ] {
            let answer = run(args.as_object().unwrap(), &context, &|| false);
            assert!(matches!(answer, NativeRun::Fallback(_)), "{args}");
            assert_eq!(
                before(),
                snapshot,
                "{args} changed the disk before handing back"
            );
        }
    }

    /// A tree with a directory the agent cannot empty goes to the Python module whole, since
    /// `shutil.rmtree`'s failure names the entry it stopped at, which the native cannot say.
    ///
    /// What would make this red: the tree removed natively, which empties what it can (`t/y`
    /// goes) and fails with another message.
    #[test]
    fn a_tree_the_agent_cannot_empty_is_handed_back_whole() {
        assert_ne!(
            unsafe { libc::geteuid() },
            0,
            "root can empty any directory"
        );
        let scratch = Scratch::new("file-tree");
        mkdir(&scratch.path("t/sub"), 0o755);
        fs::write(scratch.path("t/sub/x"), "x").unwrap();
        fs::write(scratch.path("t/y"), "y").unwrap();
        chmod(&scratch.path("t/sub"), 0o500);
        let answer = ask(&json!({"path": scratch.path("t"), "state": "absent"}));
        let left = after(&scratch.path("t/y"));
        chmod(&scratch.path("t/sub"), 0o755);
        assert!(matches!(answer, NativeRun::Fallback(_)));
        assert_eq!(left["exists"], true);
    }

    /// `_original_basename`, as `copy` sends it: a directory named as the destination stands for
    /// the file of that name inside it, in the answer and in `invocation`.
    ///
    /// What would make this red: the rewrite skipped (the mode set on the directory), or
    /// `invocation` still naming the directory as `path`.
    #[test]
    fn a_directory_given_with_a_basename_stands_for_the_file_inside() {
        let scratch = Scratch::new("file-basename");
        let file = scratch.fixture();
        let args = json!({"dest": scratch.0, "_original_basename": "f.txt", "state": "file",
                          "mode": "0600", "recurse": false});
        let NativeRun::Done(result) = ask(&args) else {
            panic!("handed back")
        };
        assert_eq!(result.0["path"], file.as_str());
        assert_eq!(result.0["changed"], true);
        assert_eq!(result.0["invocation"]["module_args"]["path"], file.as_str());
        assert_eq!(
            result.0["invocation"]["module_args"]["dest"],
            scratch.0.as_str()
        );
        assert_eq!(after(&file)["mode"], "0600");
        assert_eq!(after(&scratch.0)["mode"], "0755");
    }
}
