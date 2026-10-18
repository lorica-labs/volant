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
    Account, ArgSpec, FsError, ModeError, add_path_info, bool_param, check_names, clock, failure,
    flag, group_account, module_args, native_run, null, os_error, owner_account, parse_mode,
    path_param, realpath, selinux_enabled, set_fs_attributes, str_param,
};
use super::runtime::{Clock, Stop as Halt};
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
        // A choice the native leaves to the module is named; anything else the task gave is not.
        other @ ("hard" | "link" | "touch") => return Err(format!("state {other}").into()),
        _ => return Err("state is not one of the module's choices".into()),
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
    if !super::common::access(dir, libc::R_OK | libc::W_OK | libc::X_OK) {
        return back(format!("{dir} cannot be emptied"));
    }
    if meta.protected() {
        return back(format!("{dir} is immutable or append-only"));
    }
    let sticky = u32::from(meta.stx_mode) & 0o1000 != 0 && euid != 0 && meta.stx_uid != euid;
    let entries = fs::read_dir(dir).map_err(|err| Halt::HandBack(err.to_string()))?;
    for entry in entries {
        super::common::check(clock)?;
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

#[cfg(target_os = "linux")]
const STATX_ATTR_IMMUTABLE: u64 = 0x10;
#[cfg(target_os = "linux")]
const STATX_ATTR_APPEND: u64 = 0x20;
#[cfg(target_os = "linux")]
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
mod tests;
