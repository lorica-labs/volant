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

use super::common::{
    Account, ArgSpec, Clock, FsError, ModeError, Stop as Halt, access, add_path_info, bool_param,
    check, check_names, clock, failure, group_account, module_args, native_run, os_error,
    owner_account, parse_mode, path_param, realpath, selinux_enabled, set_fs_attributes, str_param,
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
    /// The task's `timeout` ran out, or the controller cancelled it.
    Clock(Halt),
}

impl From<String> for Stop {
    fn from(reason: String) -> Self {
        Stop::Back(reason)
    }
}

impl From<Halt> for Stop {
    fn from(halt: Halt) -> Self {
        match halt {
            Halt::HandBack(reason) => Stop::Back(reason),
            other => Stop::Clock(other),
        }
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

fn run(args: &Map<String, Value>, context: &Context, cancelled: &dyn Fn() -> bool) -> NativeRun {
    native_run(answer(args, context, clock(context, cancelled)), context)
}

fn answer(
    args: &Map<String, Value>,
    context: &Context,
    clock: Clock,
) -> Result<Map<String, Value>, Halt> {
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
        return Err(format!("{name} is set").into());
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
        return Err(why.into());
    }
    let attrs = Attrs {
        owner: str_param(&params, "owner")?
            .map(|owner| owner_account(owner, clock))
            .transpose()?,
        group: str_param(&params, "group")?
            .map(|group| group_account(group, clock))
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
        "absent" => ensure_absent(&path, clock),
        "file" => ensure_file(&path, follow, &attrs),
        "directory" => ensure_directory(&path, follow, &attrs),
        other => return Err(format!("state {other}").into()),
    };
    let mut result = match outcome {
        Ok(result) => result,
        Err(Stop::Fail(mut fail)) => {
            fail.insert("failed".into(), Value::Bool(true));
            fail
        }
        Err(Stop::Back(reason)) => return Err(Halt::HandBack(reason)),
        Err(Stop::Clock(halt)) => return Err(halt),
    };
    add_path_info(&mut result, clock)?;
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
fn ensure_absent(path: &str, clock: Clock) -> Result<Map<String, Value>, Stop> {
    let mut result = Map::new();
    result.insert("path".into(), path.into());
    result.insert("state".into(), "absent".into());
    match get_state(path)? {
        "absent" => {
            result.insert("changed".into(), Value::Bool(false));
            return Ok(result);
        }
        "directory" => {
            // `shutil.rmtree`'s failure names the entry it stopped at, and how depends on the
            // host's Python: a tree where removal could fail goes to the Python module whole.
            removable(path, clock)?;
            // `remove_dir_all` descends through directory handles and never follows a link,
            // as `rmtree` does; it does not say which entry failed, so the message names the top.
            fs::remove_dir_all(path).map_err(|err| {
                Stop::Fail(message(format!("rmtree failed: {}", os_error(&err, path))))
            })?;
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

/// Hands back unless removing the tree at `top` can only fail where the reference's message
/// names what the native's does: every directory listable, writable and searchable; no mount
/// point under `top` (another device, or a mount root on the same one); no entry marked
/// immutable or append-only; and, for an agent that is not root, no sticky directory holding
/// another account's entry in a directory that account does not own.
///
/// Every entry is read with one `statx` that does not follow links and opens nothing; the clock
/// is looked at before each one. A file system that does not report the `i` and `a` attributes
/// through `statx` is taken to have none.
#[cfg(target_os = "linux")]
fn removable(top: &str, clock: Clock) -> Result<(), Halt> {
    let meta = statx(top)?;
    walk(top, &meta, &meta, unsafe { libc::geteuid() }, clock)
}

#[cfg(not(target_os = "linux"))]
fn removable(_: &str, _: Clock) -> Result<(), Halt> {
    Err(Halt::HandBack(
        "the removal check reads Linux attributes".into(),
    ))
}

#[cfg(target_os = "linux")]
fn walk(dir: &str, meta: &Statx, top: &Statx, euid: u32, clock: Clock) -> Result<(), Halt> {
    let back = |why: String| Err(Halt::HandBack(why));
    if !access(dir, libc::R_OK | libc::W_OK | libc::X_OK) {
        return back(format!("{dir} cannot be emptied"));
    }
    if meta.protected() {
        return back(format!("{dir} is immutable or append-only"));
    }
    let sticky = u32::from(meta.stx_mode) & 0o1000 != 0 && euid != 0 && meta.stx_uid != euid;
    let entries = fs::read_dir(dir).map_err(|err| Halt::HandBack(err.to_string()))?;
    for entry in entries {
        check(clock)?;
        let entry = entry.map_err(|err| Halt::HandBack(err.to_string()))?;
        let path = entry.path();
        let path = path
            .to_str()
            .ok_or_else(|| Halt::HandBack("a name is not UTF-8".into()))?;
        let child = statx(path)?;
        if sticky && child.stx_uid != euid {
            return back(format!(
                "{path} belongs to another account in a sticky directory"
            ));
        }
        if child.protected() {
            return back(format!("{path} is immutable or append-only"));
        }
        if u32::from(child.stx_mode) & 0o170_000 == 0o040_000 {
            if (child.stx_dev_major, child.stx_dev_minor) != (top.stx_dev_major, top.stx_dev_minor)
                || child.has(STATX_ATTR_MOUNT_ROOT)
            {
                return back(format!("{path} is a mount point"));
            }
            walk(path, &child, top, euid, clock)?;
        }
    }
    Ok(())
}

const STATX_ATTR_IMMUTABLE: u64 = 0x10;
const STATX_ATTR_APPEND: u64 = 0x20;
const STATX_ATTR_MOUNT_ROOT: u64 = 0x2000;

/// The kernel's `struct statx`, up to the fields read here: `libc` only declares it for glibc
/// and a newer musl than the agent's target.
#[cfg(target_os = "linux")]
#[repr(C)]
struct Statx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    pad: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    times: [u64; 8],
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    spare: [u64; 14],
}

#[cfg(target_os = "linux")]
impl Statx {
    /// An attribute the file system reports and the entry carries.
    fn has(&self, attribute: u64) -> bool {
        self.stx_attributes_mask & self.stx_attributes & attribute != 0
    }

    fn protected(&self) -> bool {
        self.has(STATX_ATTR_IMMUTABLE) || self.has(STATX_ATTR_APPEND)
    }
}

/// `statx(path, AT_SYMLINK_NOFOLLOW, STATX_BASIC_STATS)`.
#[cfg(target_os = "linux")]
fn statx(path: &str) -> Result<Statx, Halt> {
    let name = std::ffi::CString::new(path).map_err(|err| Halt::HandBack(err.to_string()))?;
    // SAFETY: an all-zero `Statx` is a valid value of a plain integer struct.
    let mut buf: Statx = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is NUL-terminated and `buf` is a 256-byte `struct statx` the call fills.
    let done = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            name.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            0x7ff_u32,
            &raw mut buf,
        )
    };
    if done != 0 {
        return Err(Halt::HandBack(format!(
            "statx {path}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(buf)
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

    use super::super::common::golden::{Scratch, differences, within};
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
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped as root: file-chown-denied needs a chown to be refused");
            return;
        }
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
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped as root: root can empty any directory");
            return;
        }
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

    /// A tree holding what `rmtree` can fail on halfway goes to the Python module before
    /// anything is removed: an append-only file, another account's file in a sticky directory,
    /// and a mount point. All need `sudo -n` to set up, and the test says so and stops without
    /// it.
    ///
    /// What would make this red: the attribute, sticky or mount check dropped, after which the
    /// native removes what it can (`y` goes) and fails naming another entry than the reference
    /// might.
    #[test]
    fn a_tree_rmtree_would_fail_in_is_handed_back_whole() {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped as root: the sticky case needs another account than the agent's");
            return;
        }
        let sudo = |args: &[&str]| {
            std::process::Command::new("sudo")
                .arg("-n")
                .args(args)
                .status()
                .is_ok_and(|status| status.success())
        };
        let scratch = Scratch::new("file-rmtree");
        mkdir(&scratch.path("a"), 0o755);
        fs::write(scratch.path("a/y"), "y").unwrap();
        fs::write(scratch.path("a/log"), "log").unwrap();
        mkdir(&scratch.path("s"), 0o755);
        fs::write(scratch.path("s/y"), "y").unwrap();
        mkdir(&scratch.path("m/mnt"), 0o755);
        fs::write(scratch.path("m/y"), "y").unwrap();
        let (log, sticky, mnt) = (
            scratch.path("a/log"),
            scratch.path("s/t"),
            scratch.path("m/mnt"),
        );
        let clean = || {
            sudo(&["chattr", "-a", &log]);
            sudo(&["rm", "-rf", &sticky]);
            sudo(&["umount", &mnt]);
        };
        if !sudo(&["chattr", "+a", &log])
            || !sudo(&["mkdir", "-m", "1777", &sticky])
            || !sudo(&["touch", &format!("{sticky}/theirs")])
            || !sudo(&["mount", "-t", "tmpfs", "volant-test", &mnt])
        {
            clean();
            eprintln!(
                "skipped: sudo -n cannot set up an append-only file, a sticky directory and a mount"
            );
            return;
        }
        let answers =
            ["a", "s", "m"].map(|dir| ask(&json!({"path": scratch.path(dir), "state": "absent"})));
        let left = ["a/y", "s/y", "m/y"].map(|file| after(&scratch.path(file))["exists"].clone());
        clean();
        for (answer, case) in answers.iter().zip(["append-only", "sticky", "mount"]) {
            assert!(matches!(answer, NativeRun::Fallback(_)), "{case}");
        }
        assert_eq!(left, [true, true, true].map(Value::from));
    }

    /// The scan before a removal looks at the cancel for each entry, so a flat directory of a
    /// million files still ends when the controller cancels the task.
    ///
    /// What would make this red: the clock looked at once per directory, which lets the scan
    /// of `flat` finish.
    #[test]
    fn the_scan_before_a_removal_stops_at_the_cancel() {
        let scratch = Scratch::new("file-scan-cancel");
        mkdir(&scratch.path("flat"), 0o755);
        fs::write(scratch.path("flat/x"), "x").unwrap();
        fs::write(scratch.path("flat/y"), "y").unwrap();
        let polls = std::cell::Cell::new(0);
        let second_poll = || {
            polls.set(polls.get() + 1);
            polls.get() > 1
        };
        let clock = Clock {
            deadline: None,
            cancelled: &second_poll,
        };
        let top = scratch.path("flat");
        assert!(matches!(removable(&top, clock), Err(Halt::Cancelled)));
    }

    /// A link inside the tree to a directory outside it is removed as a link: the directory it
    /// points to, and what it holds, stay.
    ///
    /// What would make this red: a removal that follows links, which empties `outside`.
    #[test]
    fn a_link_out_of_the_tree_is_removed_not_followed() {
        let scratch = Scratch::new("file-link-out");
        mkdir(&scratch.path("outside"), 0o755);
        fs::write(scratch.path("outside/keep"), "keep").unwrap();
        mkdir(&scratch.path("t/d"), 0o755);
        fs::write(scratch.path("t/d/x"), "x").unwrap();
        std::os::unix::fs::symlink(scratch.path("outside"), scratch.path("t/sub")).unwrap();
        let answer = ask(&json!({"path": scratch.path("t"), "state": "absent"}));
        assert!(matches!(answer, NativeRun::Done(_)));
        assert_eq!(after(&scratch.path("t"))["exists"], false);
        assert_eq!(after(&scratch.path("outside/keep"))["content"], "keep");
    }

    /// The task's `timeout` reaches a name service that hangs: a `getent` that sleeps, first on
    /// `PATH`, for an owner `/etc/passwd` does not hold. The answer is the one the Python path
    /// gives for a module that outlives the task, and nothing was changed.
    ///
    /// What would make this red: `getent` run outside `command`'s executor (the test waits for
    /// the sleep, 600 s, and nextest ends it), or the timeout answered as anything else.
    #[test]
    fn a_hung_name_service_ends_with_the_timeout() {
        let scratch = Scratch::new("file-hung");
        let file = scratch.fixture();
        super::super::common::golden::fake_getent(&scratch, "exec sleep 600");
        let context = Context {
            timeout: Some(std::time::Duration::from_secs(1)),
            ..Context::default()
        };
        let args = json!({"path": file, "owner": "volant-no-such-user", "mode": "0600"});
        let timed = args.clone();
        let answer = within(30, move || {
            run(timed.as_object().unwrap(), &context, &|| false)
        });
        let NativeRun::Done(result) = answer else {
            panic!("no answer")
        };
        assert_eq!(result, volant_protocol::TaskResult::timed_out(1));
        assert_eq!(after(&file)["mode"], "0644");
        let cancelled = within(30, move || {
            run(args.as_object().unwrap(), &Context::default(), &|| true)
        });
        assert!(matches!(cancelled, NativeRun::Cancelled));
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
