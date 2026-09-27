// SPDX-License-Identifier: GPL-3.0-or-later
//! What every native shares with the reference's module machinery, `AnsibleModule` of
//! ansible-core 2.19.12 (`module_utils/basic.py`) for the most part.
//!
//! The rule every function here serves: a native answers only where it can say what the
//! reference would say. Where that cannot be known without the Python module, a function
//! returns the reason as an `Err(String)` (or `Stop::HandBack`), and the native hands the task
//! back with it, before anything on the host changed. Whatever runs a program or can take long
//! does so under a `Clock`, the task's `timeout` and the controller's cancel.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

pub use super::setup::{Clock, Stop, run as run_program};

/// `Stop::TimedOut` once the task's deadline has passed, `Stop::Cancelled` once the controller
/// has cancelled it: for a loop of the native's own that can run long.
pub fn check(clock: Clock) -> Result<(), Stop> {
    if clock
        .deadline
        .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    {
        return Err(Stop::TimedOut);
    }
    if (clock.cancelled)() {
        return Err(Stop::Cancelled);
    }
    Ok(())
}

/// What a native's answer becomes: the result, the hand-back, or, when the task's `timeout`
/// ran out, what the Python path answers when the module outlives it.
pub fn native_run(
    answer: Result<Map<String, Value>, Stop>,
    context: &crate::modules::Context,
) -> super::NativeRun {
    use super::NativeRun;
    match answer {
        Ok(result) => NativeRun::Done(volant_protocol::TaskResult(result)),
        Err(Stop::HandBack(reason)) => NativeRun::Fallback(reason),
        Err(Stop::TimedOut) => NativeRun::Done(volant_protocol::TaskResult::timed_out(
            context.timeout.unwrap_or_default().as_secs(),
        )),
        Err(Stop::Cancelled) => NativeRun::Cancelled,
    }
}

/// The clock of a task: its `timeout` from now, and the controller's cancel.
pub fn clock<'a>(context: &crate::modules::Context, cancelled: &'a dyn Fn() -> bool) -> Clock<'a> {
    Clock {
        deadline: context
            .timeout
            .map(|timeout| std::time::Instant::now() + timeout),
        cancelled,
    }
}

/// One entry of a module's `argument_spec`, copied from ansible-core 2.19.12.
pub struct ArgSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// The reference's default, `Value::Null` where it has none.
    pub default: fn() -> Value,
}

/// An argument without a default.
pub const fn null(name: &'static str) -> ArgSpec {
    ArgSpec {
        name,
        aliases: &[],
        default: || Value::Null,
    }
}

/// A `type='bool'` argument and its default.
pub const fn flag(name: &'static str, default: bool) -> ArgSpec {
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

/// The `invocation` a native returns: `{"module_args": ...}` holding every argument of `spec`,
/// the way `AnsibleModule` fills its validated parameters.
///
/// An alias sets its canonical argument and stays in the map under its own name too; when
/// several spellings are given, the last alias in the spec's list wins, over the canonical name
/// as well. An argument nobody gave gets its default, `null` included. Measured on
/// ansible-core 2.19.12: `stat` given `dest` returns both `dest` and `path` in `module_args`.
///
/// Arguments outside the spec are left as given: refusing them is the native's decision, made
/// before it answers at all.
pub fn invocation(spec: &[ArgSpec], args: &Map<String, Value>) -> Value {
    let mut invocation = Map::new();
    invocation.insert("module_args".into(), Value::Object(module_args(spec, args)));
    Value::Object(invocation)
}

/// The validated parameters `invocation` wraps, for a native that reads them, or rewrites one
/// as the reference's module does to its own `params` before it answers.
pub fn module_args(spec: &[ArgSpec], args: &Map<String, Value>) -> Map<String, Value> {
    let mut module_args = args.clone();
    for arg in spec {
        for alias in arg.aliases {
            if let Some(value) = args.get(*alias) {
                module_args.insert(arg.name.to_string(), value.clone());
            }
        }
        if !module_args.contains_key(arg.name) {
            module_args.insert(arg.name.to_string(), (arg.default)());
        }
    }
    module_args
}

/// Hands the task back unless every argument is one of `spec`'s, given under one name.
///
/// An unknown argument fails the reference's validation, and an option given under two of its
/// names adds a warning (`Both option path and its alias dest are set.`): the Python module says
/// either better than a native would.
pub fn check_names(spec: &[ArgSpec], args: &Map<String, Value>) -> Result<(), String> {
    for key in args.keys() {
        if !spec
            .iter()
            .any(|arg| arg.name == key || arg.aliases.contains(&key.as_str()))
        {
            return Err(format!("argument {key} is unknown here"));
        }
    }
    for arg in spec {
        let names = std::iter::once(arg.name).chain(arg.aliases.iter().copied());
        if names.filter(|name| args.contains_key(*name)).count() > 1 {
            return Err(format!("{} is given under more than one name", arg.name));
        }
    }
    Ok(())
}

/// A `type='bool'` parameter, taken only as a JSON boolean. The reference also converts `"yes"`,
/// `1` and the like, and shows the converted value in `invocation`; the Python module does that.
pub fn bool_param(params: &Map<String, Value>, name: &str) -> Result<bool, String> {
    params
        .get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("{name} is not a boolean"))
}

/// A `type='str'` parameter: `None` when null, taken only as a JSON string otherwise (the
/// reference converts a number and warns about it).
pub fn str_param<'a>(
    params: &'a Map<String, Value>,
    name: &str,
) -> Result<Option<&'a str>, String> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("{name} is not a string")),
    }
}

/// A required `type='path'` parameter. The reference expands `~` and `$VAR` in it and resolves a
/// relative one against the module's own working directory; a native takes only an absolute path
/// that none of that changes.
pub fn path_param<'a>(params: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    match str_param(params, name)? {
        Some(path) if path.starts_with('/') && !path.contains(['$', '\0']) => Ok(path),
        _ => Err(format!("{name} is not a plain absolute path")),
    }
}

/// Whether SELinux is on, which puts a security context into the reference's results and file
/// operations. A native hands the task back when it is.
pub fn selinux_enabled() -> bool {
    Path::new("/sys/fs/selinux/enforce").exists()
}

/// An account of `/etc/passwd`, or of the host's name service.
pub struct Passwd {
    pub name: String,
    pub uid: u32,
}

/// A group of `/etc/group`, or of the host's name service.
pub struct Group {
    pub name: String,
    pub gid: u32,
}

/// The account `name_or_uid` names, by uid when it is all digits, as `pwd.getpwnam` or
/// `pwd.getpwuid` would find it: `Ok(None)` when the host has no such account, `Err` when that
/// cannot be told.
///
/// The agent is a static musl binary, with no NSS. `/etc/passwd` answers when `nsswitch.conf`
/// puts `files` first; an account it does not hold is asked of `getent`, which runs through the
/// host's own C library and so sees LDAP, sssd or systemd's dynamic users as Python does.
///
/// `getent` runs through `command`'s executor under `clock`: the task's `timeout` and cancel
/// reach a name service that hangs, as they reach the Python module's own lookup.
pub fn lookup_user(name_or_uid: &str, clock: Clock) -> Result<Option<Passwd>, Stop> {
    Ok(lookup("passwd", "/etc/passwd", 7, name_or_uid, clock)?
        .map(|(name, uid)| Passwd { name, uid }))
}

/// The group `name_or_gid` names, as `lookup_user` finds an account.
pub fn lookup_group(name_or_gid: &str, clock: Clock) -> Result<Option<Group>, Stop> {
    Ok(
        lookup("group", "/etc/group", 4, name_or_gid, clock)?
            .map(|(name, gid)| Group { name, gid }),
    )
}

/// The name and id of the entry `key` names in the database `db`, read from `file` then from
/// `getent`. An entry is `fields` colon-separated fields with the id third.
fn lookup(
    db: &str,
    file: &str,
    fields: usize,
    key: &str,
    clock: Clock,
) -> Result<Option<(String, u32)>, Stop> {
    let id = if !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()) {
        Some(
            key.parse::<u32>()
                .map_err(|_| format!("{db} id {key} is out of range"))?,
        )
    } else {
        None
    };
    let entry = |line: &str| -> Option<(String, u32)> {
        let parts: Vec<&str> = line.split(':').collect();
        let entry_id = parts.get(2)?.parse::<u32>().ok()?;
        let hit = match id {
            Some(id) => entry_id == id,
            None => parts[0] == key,
        };
        (parts.len() >= fields && hit).then(|| (parts[0].to_string(), entry_id))
    };
    if files_first(db) {
        let text = fs::read_to_string(file).map_err(|err| format!("reading {file}: {err}"))?;
        for line in text.lines() {
            if line.starts_with(['+', '-']) {
                return Err(format!("{file} has NIS compat entries").into());
            }
            if let Some(found) = entry(line) {
                return Ok(Some(found));
            }
        }
    }
    let no_env = BTreeMap::new();
    match run_program(&no_env, clock, Path::new("getent"), &[db, key])? {
        Some((0, out)) => Ok(Some(out.lines().find_map(entry).ok_or_else(|| {
            Stop::HandBack(format!("getent {db} answered no entry for {key}"))
        })?)),
        Some((2, _)) => Ok(None),
        Some((rc, _)) => Err(Stop::HandBack(format!("getent {db} exited {rc}"))),
        None => Err(Stop::HandBack("getent cannot be run".into())),
    }
}

/// Whether `nsswitch.conf` asks `files` first for `db`, so that an entry found there is the one
/// the C library returns. glibc's default, without the file, is `files` first as well.
pub(crate) fn files_first(db: &str) -> bool {
    let Ok(conf) = fs::read_to_string("/etc/nsswitch.conf") else {
        return true;
    };
    conf.lines()
        .filter_map(|line| line.split('#').next()?.trim().strip_prefix(db))
        .find_map(|rest| rest.trim_start().strip_prefix(':'))
        .is_none_or(|sources| matches!(sources.split_whitespace().next(), Some("files" | "compat")))
}

/// An `owner` or `group` argument, resolved before anything changes.
pub enum Account {
    Id(u32),
    /// No such name on the host: the reference fails when it reaches this attribute.
    Unknown(String),
}

/// `owner` as `set_owner_if_different` reads it: a number is a uid, anything else a name.
pub fn owner_account(owner: &str, clock: Clock) -> Result<Account, Stop> {
    account(owner, |name| Ok(lookup_user(name, clock)?.map(|p| p.uid)))
}

/// `group` as `set_group_if_different` reads it.
pub fn group_account(group: &str, clock: Clock) -> Result<Account, Stop> {
    account(group, |name| Ok(lookup_group(name, clock)?.map(|g| g.gid)))
}

fn account(given: &str, find: impl Fn(&str) -> Result<Option<u32>, Stop>) -> Result<Account, Stop> {
    if !given.is_empty() && given.bytes().all(|b| b.is_ascii_digit()) {
        return given
            .parse()
            .map(Account::Id)
            .map_err(|_| Stop::HandBack(format!("id {given} is out of range")));
    }
    // Python's `int()` also takes a sign, underscores, spaces and other scripts' digits.
    if given.is_empty()
        || (given.chars().any(char::is_numeric)
            && given
                .chars()
                .all(|c| c.is_numeric() || c.is_whitespace() || "+-_".contains(c)))
    {
        return Err(Stop::HandBack(format!(
            "{given:?} is read as a number or not at all by Python"
        )));
    }
    Ok(find(given)?.map_or_else(|| Account::Unknown(given.to_string()), Account::Id))
}

/// Why a `mode` gives no permission bits.
#[derive(Debug, PartialEq)]
pub enum ModeError {
    /// The reference fails with `mode must be in octal or symbolic form`; this is its `details`.
    Invalid(String),
    /// A form the reference reads some other way (`0o755`, `+755`, a float, `True`...): the
    /// Python module decides.
    Unsupported(String),
}

/// The permission bits `mode` asks for, as `set_mode_if_different` computes them: an integer
/// as it is, a string of octal digits in base 8, anything else as a symbolic mode
/// (`_symbolic_mode_to_octal`) applied to `current`, the path's permission bits now.
pub fn parse_mode(mode: &Value, current: u32, is_dir: bool) -> Result<u32, ModeError> {
    let unsupported = || ModeError::Unsupported(format!("mode {mode}"));
    match mode {
        Value::Number(n) => n
            .as_u64()
            .filter(|bits| *bits <= 0o7777)
            .map(|bits| bits as u32)
            .ok_or_else(unsupported),
        Value::String(text)
            if !text.is_empty() && text.bytes().all(|b| (b'0'..=b'7').contains(&b)) =>
        {
            u32::from_str_radix(text, 8)
                .ok()
                .filter(|bits| *bits <= 0o7777)
                .ok_or_else(unsupported)
        }
        Value::String(text)
            if text.bytes().all(|b| b.is_ascii_graphic())
                && !(text.bytes().any(|b| b.is_ascii_digit())
                    && text
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b"_+-oO".contains(&b))) =>
        {
            symbolic_mode(text, current, is_dir)
        }
        _ => Err(unsupported()),
    }
}

/// `_symbolic_mode_to_octal`: `u=rwx,g+X,o-w` and the like, each clause applied in turn.
fn symbolic_mode(mode: &str, current: u32, is_dir: bool) -> Result<u32, ModeError> {
    let mut new = current;
    for clause in mode.split(',') {
        let invalid = || ModeError::Invalid(format!("bad symbolic permission for mode: {clause}"));
        let mut parts = clause.split(['+', '=', '-']);
        let operators: Vec<char> = clause.chars().filter(|c| "+=-".contains(*c)).collect();
        let users = parts.next().unwrap_or_default();
        let use_umask = users.is_empty();
        let users = if users.is_empty() || users == "a" {
            "ugo"
        } else {
            users
        };
        if !users.chars().all(|c| "ugo".contains(c)) {
            return Err(invalid());
        }
        for (perms, operator) in parts.zip(operators) {
            if !perms.chars().all(|c| "rwxXstugo".contains(c)) {
                return Err(invalid());
            }
            let umask = if use_umask { umask()? } else { 0 };
            for user in users.chars() {
                let bits = perms.chars().fold(0, |acc, perm| {
                    let masked = if "rwx".contains(perm) { !umask } else { 0o7777 };
                    acc | (perm_bits(user, perm, new, is_dir) & masked)
                });
                new = match operator {
                    '=' => {
                        let mask = match user {
                            'u' => 0o4700,
                            'g' => 0o2070,
                            _ => 0o1007,
                        };
                        (new & (mask ^ 0o7777)) | bits
                    }
                    '+' => new | bits,
                    _ => new & !bits,
                };
            }
        }
    }
    Ok(new)
}

/// What one letter of a symbolic mode means for `user`, `prev` being the bits so far
/// (`_get_octal_mode_from_symbolic_perms`).
fn perm_bits(user: char, perm: char, prev: u32, is_dir: bool) -> u32 {
    // Where `user`'s three bits sit: 6 for u, 3 for g, 0 for o.
    let shift = match user {
        'u' => 6,
        'g' => 3,
        _ => 0,
    };
    let copy = |from: u32| ((prev >> from) & 0o7) << shift;
    match perm {
        'r' => 0o4 << shift,
        'w' => 0o2 << shift,
        'x' => 0o1 << shift,
        'X' if is_dir || prev & 0o111 != 0 => 0o1 << shift,
        's' if user == 'u' => 0o4000,
        's' if user == 'g' => 0o2000,
        't' if user == 'o' => 0o1000,
        'u' => copy(6),
        'g' => copy(3),
        'o' => copy(0),
        _ => 0,
    }
}

/// The process umask, read without setting it (`/proc/self/status`, Linux 4.7 and later): the
/// agent's threads would see a temporary `umask(0)`.
pub fn umask() -> Result<u32, ModeError> {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let value = status
                .lines()
                .find_map(|line| line.strip_prefix("Umask:"))?;
            u32::from_str_radix(value.trim(), 8).ok()
        })
        .ok_or_else(|| ModeError::Unsupported("the umask cannot be read".into()))
}

/// Why `set_fs_attributes` stopped.
#[derive(Debug)]
pub enum FsError {
    /// The reference's `fail_json`, with these keys (`msg`, `path`, maybe `details`).
    Failed(Map<String, Value>),
    /// The reference's module would end on this exception, as Python prints it; `true` when an
    /// owner or group was changed before it.
    Raised(String, bool),
}

/// `set_fs_attributes_if_different` without SELinux or `attributes`: owner, then group, then
/// mode, each read from the path again, each changed only when it differs, and the first failure
/// stops the rest, the attributes already set staying set. Returns whether anything changed.
pub fn set_fs_attributes(
    path: &str,
    owner: Option<&Account>,
    group: Option<&Account>,
    mode: Option<&Value>,
) -> Result<bool, FsError> {
    set_fs_attributes_diff(path, owner, group, mode, &mut Map::new())
}

/// `set_fs_attributes`, filling `diff` as the reference fills its `diff` argument: `before` and
/// `after` maps holding the `owner` and `group` ids and the `mode` text of each attribute about
/// to change, written before the change is tried.
pub fn set_fs_attributes_diff(
    path: &str,
    owner: Option<&Account>,
    group: Option<&Account>,
    mode: Option<&Value>,
    diff: &mut Map<String, Value>,
) -> Result<bool, FsError> {
    let mut record = |key: &str, before: Value, after: Value| {
        for (side, value) in [("before", before), ("after", after)] {
            if let Value::Object(map) = diff
                .entry(side)
                .or_insert_with(|| Value::Object(Map::new()))
            {
                map.insert(key.to_string(), value);
            }
        }
    };
    let mut changed = false;
    let lstat = |changed: bool| {
        fs::symlink_metadata(path).map_err(|err| FsError::Raised(os_error(&err, path), changed))
    };
    let lookup_failed = |msg: String| Err(FsError::Failed(failure(path, msg)));
    if let Some(owner) = owner {
        let current = lstat(changed)?.uid();
        let uid = match owner {
            Account::Id(uid) => *uid,
            Account::Unknown(name) => {
                return lookup_failed(format!("chown failed: failed to look up user {name}"));
            }
        };
        if current != uid {
            record("owner", current.into(), uid.into());
            std::os::unix::fs::lchown(path, Some(uid), None)
                .map_err(|_| FsError::Failed(failure(path, "chown failed".into())))?;
            changed = true;
        }
    }
    if let Some(group) = group {
        let current = lstat(changed)?.gid();
        let gid = match group {
            Account::Id(gid) => *gid,
            Account::Unknown(name) => {
                return lookup_failed(format!("chgrp failed: failed to look up group {name}"));
            }
        };
        if current != gid {
            record("group", current.into(), gid.into());
            std::os::unix::fs::lchown(path, None, Some(gid))
                .map_err(|_| FsError::Failed(failure(path, "chgrp failed".into())))?;
            changed = true;
        }
    }
    if let Some(mode) = mode {
        let meta = lstat(changed)?;
        let prev = meta.mode() & 0o7777;
        let mode = match parse_mode(mode, prev, meta.is_dir()) {
            Ok(mode) => mode,
            Err(ModeError::Invalid(details)) => {
                let mut fail = failure(path, "mode must be in octal or symbolic form".into());
                fail.insert("details".into(), Value::String(details));
                return Err(FsError::Failed(fail));
            }
            Err(ModeError::Unsupported(why)) => return Err(FsError::Raised(why, changed)),
        };
        if prev != mode {
            record(
                "mode",
                format!("0{prev:03o}").into(),
                format!("0{mode:03o}").into(),
            );
            // Python's chmod of a link goes through it and puts the target back; no native
            // sets a mode on a link.
            if meta.file_type().is_symlink() {
                return Err(FsError::Raised(
                    format!("{path} is a symbolic link"),
                    changed,
                ));
            }
            match fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
                Err(err) if !matches!(err.raw_os_error(), Some(libc::ENOENT | libc::ELOOP)) => {
                    return Err(FsError::Raised(os_error(&err, path), changed));
                }
                _ => {}
            }
            changed |= lstat(changed)?.mode() & 0o7777 != prev;
        }
    }
    Ok(changed)
}

/// The keys of a `fail_json(path=path, msg=msg)`, before `failed` and `add_path_info`.
pub fn failure(path: &str, msg: String) -> Map<String, Value> {
    let mut fail = Map::new();
    fail.insert("msg".into(), Value::String(msg));
    fail.insert("path".into(), Value::String(path.to_string()));
    fail
}

/// `AnsibleModule.add_path_info`: what the reference adds to a result naming an existing `path`
/// (or `dest`): owner and group by id and by name, mode, kind and size.
///
/// A name the host cannot resolve reads as the id, as it does in the reference when there is no
/// such account; so does one whose lookup failed, which a native rules out before acting.
///
/// A lookup that outlives the task's `timeout`, or that the controller cancels, ends the answer
/// the way the Python module's would end.
pub fn add_path_info(result: &mut Map<String, Value>, clock: Clock) -> Result<(), Stop> {
    let path = match result.get("path") {
        Some(path) => path,
        None => match result.get("dest") {
            Some(dest) => dest,
            None => return Ok(()),
        },
    };
    let Some(path) = path.as_str().map(str::to_string) else {
        return Ok(());
    };
    let (Ok(stat), Ok(lstat)) = (fs::metadata(&path), fs::symlink_metadata(&path)) else {
        return Ok(());
    };
    let (uid, gid) = (lstat.uid(), lstat.gid());
    let name = |found: Result<Option<String>, Stop>, id: u32| match found {
        Ok(Some(name)) => Ok(name),
        Ok(None) | Err(Stop::HandBack(_)) => Ok(id.to_string()),
        Err(stop) => Err(stop),
    };
    let owner = name(
        lookup_user(&uid.to_string(), clock).map(|p| p.map(|p| p.name)),
        uid,
    )?;
    let group = name(
        lookup_group(&gid.to_string(), clock).map(|g| g.map(|g| g.name)),
        gid,
    )?;
    let state = if lstat.file_type().is_symlink() {
        "link"
    } else if stat.is_dir() {
        "directory"
    } else if stat.nlink() > 1 {
        "hard"
    } else {
        "file"
    };
    result.insert("uid".into(), uid.into());
    result.insert("gid".into(), gid.into());
    result.insert("owner".into(), owner.into());
    result.insert("group".into(), group.into());
    result.insert(
        "mode".into(),
        format!("0{:03o}", lstat.mode() & 0o7777).into(),
    );
    result.insert("state".into(), state.into());
    result.insert("size".into(), lstat.size().into());
    Ok(())
}

/// Python's `str()` of the `OSError` an `os` call on the bytes path `path` raises:
/// `[Errno 13] Permission denied: b'/x'`, with glibc's wording, which is what the reference's
/// interpreter prints on the hosts a native answers for.
pub fn os_error(err: &io::Error, path: &str) -> String {
    let Some(errno) = err.raw_os_error() else {
        return err.to_string();
    };
    let text = strerror(errno).map_or_else(|| err.to_string(), str::to_string);
    format!("[Errno {errno}] {text}: {}", bytes_repr(path))
}

/// glibc's `strerror` for the errors file operations meet.
pub fn strerror(errno: i32) -> Option<&'static str> {
    Some(match errno {
        libc::EPERM => "Operation not permitted",
        libc::ENOENT => "No such file or directory",
        libc::EIO => "Input/output error",
        libc::EACCES => "Permission denied",
        libc::EBUSY => "Device or resource busy",
        libc::EEXIST => "File exists",
        libc::ENOTDIR => "Not a directory",
        libc::EISDIR => "Is a directory",
        libc::ENOSPC => "No space left on device",
        libc::EROFS => "Read-only file system",
        libc::ENAMETOOLONG => "File name too long",
        libc::ENOTEMPTY => "Directory not empty",
        libc::ELOOP => "Too many levels of symbolic links",
        libc::EDQUOT => "Disk quota exceeded",
        _ => return None,
    })
}

/// Python's `repr()` of `text` encoded as bytes.
fn bytes_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut repr = format!("b{quote}");
    for byte in text.bytes() {
        match byte {
            b'\\' => repr.push_str("\\\\"),
            b'\t' => repr.push_str("\\t"),
            b'\n' => repr.push_str("\\n"),
            b'\r' => repr.push_str("\\r"),
            _ if byte == quote as u8 => {
                repr.push('\\');
                repr.push(quote);
            }
            0x20..0x7f => repr.push(byte as char),
            _ => {
                let _ = write!(repr, "\\x{byte:02x}");
            }
        }
    }
    repr.push(quote);
    repr
}

/// `os.access(path, mode)`: by the real uid, following links.
pub fn access(path: &str, mode: libc::c_int) -> bool {
    CString::new(path).is_ok_and(|path| unsafe { libc::access(path.as_ptr(), mode) } == 0)
}

/// `get_bin_path`: the first executable `name` on `PATH`, then in the `sbin` directories.
pub fn bin_path(name: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut dirs: Vec<&str> = path.split(':').collect();
    for sbin in ["/sbin", "/usr/sbin", "/usr/local/sbin"] {
        if !dirs.contains(&sbin) && Path::new(sbin).exists() {
            dirs.push(sbin);
        }
    }
    dirs.into_iter()
        .filter(|dir| !dir.is_empty())
        .map(|dir| format!("{}/{name}", dir.trim_end_matches('/')))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|meta| !meta.is_dir() && meta.mode() & 0o111 != 0)
        })
}

/// `os.path.realpath` for a path whose every link resolves; `Err` for one that does not, where
/// Python's non-strict answer would need reproducing.
pub fn realpath(path: &str) -> Result<String, String> {
    fs::canonicalize(path)
        .ok()
        .and_then(|real| real.to_str().map(str::to_string))
        .ok_or_else(|| format!("{path} does not resolve"))
}

/// Backups this agent has named so far.
static BACKUPS: AtomicU64 = AtomicU64::new(0);

/// Why `backup_local` made no backup.
#[derive(Debug)]
pub enum BackupError {
    /// A file already has the backup's name. Nothing was written: a native hands back.
    Exists(String),
    /// The reference's exception text; the backup may be partly written.
    Failed(String),
}

/// `backup_local`: a copy of `path` named `<path>.<number>.<%Y-%m-%d@%H:%M:%S>~`, with the
/// file's mode, times and owner (`preserved_copy`).
///
/// The reference's number is its module's pid, new for every task. A native runs inside the
/// agent, whose pid every task shares, so two backups of one file in the same second would
/// share a name. The number here is the agent's pid followed by a sequence number of this run,
/// at least four digits, never reused; and a name some file already has is never overwritten.
pub fn backup_local(path: &str) -> Result<String, BackupError> {
    let seq = BACKUPS.fetch_add(1, Ordering::Relaxed) + 1;
    let name = format!("{path}.{}{seq:04}.{}", std::process::id(), local_stamp());
    backup_copy(path, &name)?;
    Ok(name)
}

/// `preserved_copy(path, name)` into a file created for it, never over one that exists.
pub fn backup_copy(path: &str, name: &str) -> Result<(), BackupError> {
    let failed = |_: io::Error| {
        BackupError::Failed(format!("Could not make backup of '{path}' to '{name}'."))
    };
    let meta = fs::metadata(path).map_err(failed)?;
    let mut source = fs::File::open(path).map_err(failed)?;
    let mut copy = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(name)
    {
        Ok(copy) => copy,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Err(BackupError::Exists(name.to_string()));
        }
        Err(err) => return Err(failed(err)),
    };
    io::copy(&mut source, &mut copy).map_err(failed)?;
    drop(copy);
    fs::set_permissions(name, fs::Permissions::from_mode(meta.mode() & 0o7777)).map_err(failed)?;
    set_times(name, &meta).map_err(failed)?;
    chown_if_permitted(name, &meta).map_err(failed)?;
    Ok(())
}

/// `time.strftime("%Y-%m-%d@%H:%M:%S~", time.localtime())`.
fn local_stamp() -> String {
    // SAFETY: `time` accepts a null pointer; `tm` is plain data that `localtime_r` fills.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&raw const now, &raw mut tm) };
    format!(
        "{:04}-{:02}-{:02}@{:02}:{:02}:{:02}~",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// `shutil.copystat`'s times: `meta`'s access and modification times, to the nanosecond.
fn set_times(path: &str, meta: &fs::Metadata) -> io::Result<()> {
    let path = CString::new(path).map_err(io::Error::other)?;
    let times = [
        libc::timespec {
            tv_sec: meta.atime() as _,
            tv_nsec: meta.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: meta.mtime() as _,
            tv_nsec: meta.mtime_nsec() as _,
        },
    ];
    // SAFETY: `times` holds the two entries `utimensat` reads.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `os.chown(path, uid, gid)` of `meta`'s owner when it differs, a refusal ignored.
pub fn chown_if_permitted(path: &str, meta: &fs::Metadata) -> io::Result<()> {
    let now = fs::metadata(path)?;
    if (now.uid(), now.gid()) == (meta.uid(), meta.gid()) {
        return Ok(());
    }
    match std::os::unix::fs::chown(path, Some(meta.uid()), Some(meta.gid())) {
        Err(err) if err.raw_os_error() != Some(libc::EPERM) => Err(err),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[cfg(target_os = "linux")]
    use super::super::setup::unbounded;
    use super::*;

    /// `stat`'s shape against the reference: `dest` given, `path` filled from it, both kept, and
    /// every other argument at its default, `null` ones included.
    ///
    /// What would make this red: the alias dropped from the map, or the canonical name left
    /// unset, both of which the golden sees as a different `invocation`; a `null` default left
    /// out, which the reference prints; or the canonical name winning over an alias given with
    /// it, which is the reverse of what `_handle_aliases` does.
    #[test]
    fn the_invocation_resolves_aliases_and_fills_defaults_like_the_reference() {
        const SPEC: &[ArgSpec] = &[
            ArgSpec {
                name: "path",
                aliases: &["dest", "name"],
                default: || Value::Null,
            },
            ArgSpec {
                name: "follow",
                aliases: &[],
                default: || Value::Bool(false),
            },
            ArgSpec {
                name: "checksum_algorithm",
                aliases: &["checksum"],
                default: || json!("sha1"),
            },
            ArgSpec {
                name: "get_mime",
                aliases: &[],
                default: || Value::Null,
            },
        ];
        let args = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(
            invocation(SPEC, &args(json!({"dest": "/etc/hostname"}))),
            json!({"module_args": {
                "dest": "/etc/hostname",
                "path": "/etc/hostname",
                "follow": false,
                "checksum_algorithm": "sha1",
                "get_mime": null,
            }})
        );
        assert_eq!(
            invocation(
                SPEC,
                &args(json!({"path": "/a", "dest": "/b", "name": "/c"}))
            )["module_args"]["path"],
            "/c",
            "the last alias given wins, over the canonical name too"
        );
    }

    /// Unknown arguments and an option under two names go to the Python module.
    ///
    /// What would make this red: either check dropped, after which a native answers a task the
    /// reference refuses or warns about.
    #[test]
    fn an_unknown_argument_or_a_doubled_option_is_handed_back() {
        const SPEC: &[ArgSpec] = &[ArgSpec {
            name: "path",
            aliases: &["dest"],
            default: || Value::Null,
        }];
        let args = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(check_names(SPEC, &args(json!({"dest": "/a"}))), Ok(()));
        assert!(check_names(SPEC, &args(json!({"path": "/a", "dest": "/a"}))).is_err());
        assert!(check_names(SPEC, &args(json!({"path": "/a", "bogus": 1}))).is_err());
    }

    /// `parse_mode` against `AnsibleModule._symbolic_mode_to_octal` of ansible-core 2.19.12,
    /// whose answers for these modes, run under umask 022, are the expected values here.
    ///
    /// What would make this red: `X` read from the mode before the clause rather than the running
    /// one (`u=rw-x+X` gives 0o700), `=` leaving the setuid or setgid bit of its class, the
    /// umask applied to a mode that names its users, a Python-only integer form (`0o755`) read as
    /// something, or `0x755` accepted.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_mode_is_read_as_the_reference_reads_it() {
        // nextest runs each test in its own process: the umask set here reaches no other test.
        unsafe { libc::umask(0o022) };
        for (mode, current, is_dir, want) in [
            ("u=rwx,g=rx,o=", 0o755, true, 0o750),
            ("a+X", 0o644, false, 0o644),
            ("a+X", 0o644, true, 0o755),
            ("u+s,g-w,o=t", 0o775, false, 0o5750),
            ("go=u", 0o640, false, 0o666),
            ("+x", 0o600, false, 0o711),
            ("=r", 0o777, false, 0o444),
            ("+w", 0o600, false, 0o600),
            ("=rw", 0o777, false, 0o644),
            ("-w", 0o666, false, 0o466),
            ("u=rw-x+X", 0o700, false, 0o600),
            ("ug+rw,o-rwx", 0o007, true, 0o660),
            ("u", 0o640, false, 0o640),
            ("", 0o640, false, 0o640),
            ("u-rwx,g=o", 0o6754, false, 0o4044),
            ("o+t,g+s,u+s", 0o755, true, 0o7755),
            ("u+x,", 0o644, false, 0o744),
            ("0755", 0o600, false, 0o755),
            ("755", 0o600, false, 0o755),
        ] {
            assert_eq!(
                parse_mode(&json!(mode), current, is_dir),
                Ok(want),
                "{mode}"
            );
        }
        assert_eq!(parse_mode(&json!(493), 0o600, false), Ok(0o755));
        for bad in ["0x755", "u=rwz", "q+x", "rwx"] {
            assert_eq!(
                parse_mode(&json!(bad), 0o644, false),
                Err(ModeError::Invalid(format!(
                    "bad symbolic permission for mode: {bad}"
                )))
            );
        }
        for other in [
            json!("0o755"),
            json!("+755"),
            json!(" 755"),
            json!(7.5),
            json!(true),
            json!(-1),
        ] {
            assert!(
                matches!(
                    parse_mode(&other, 0o644, false),
                    Err(ModeError::Unsupported(_))
                ),
                "{other}"
            );
        }
    }

    /// Accounts and groups from `/etc/passwd` and `/etc/group`, and a name nobody has answered
    /// by `getent` as absent rather than unknown.
    ///
    /// What would make this red: a uid read as a name, or a name `/etc/passwd` lacks taken as
    /// missing without asking the name service (the `getent` step skipped would still pass this
    /// one; `the_owner_unknown_to_the_host_fails_like_the_reference` in `file` needs it).
    #[test]
    #[cfg(target_os = "linux")]
    fn accounts_are_found_by_name_and_by_id() {
        let clock = unbounded();
        assert_eq!(lookup_user("0", clock).unwrap().unwrap().name, "root");
        assert_eq!(lookup_user("root", clock).unwrap().unwrap().uid, 0);
        assert_eq!(lookup_group("0", clock).unwrap().unwrap().name, "root");
        assert_eq!(lookup_group("root", clock).unwrap().unwrap().gid, 0);
        assert!(lookup_user("volant-no-such-user", clock).unwrap().is_none());
        assert!(matches!(
            owner_account("1234", clock),
            Ok(Account::Id(1234))
        ));
        assert!(matches!(
            owner_account("volant-no-such-user", clock),
            Ok(Account::Unknown(_))
        ));
        assert!(
            owner_account("+12", clock).is_err(),
            "Python reads +12 as a number"
        );
    }

    /// An account only the name service knows, as on an LDAP or sssd host: a fake `getent` first
    /// on `PATH` answers for `volant-ldapuser` (uid 4242), whom `/etc/passwd` does not hold.
    ///
    /// What would make this red: the `getent` step dropped, or its exit 0 read as anything but
    /// the entry; `file` would then fail `chown failed: failed to look up user volant-ldapuser`
    /// where the reference chowns, and `add_path_info` would print the uid for the owner.
    #[test]
    #[cfg(target_os = "linux")]
    fn an_account_only_the_name_service_knows_is_found_through_getent() {
        let scratch = golden::Scratch::new("getent");
        golden::fake_getent(
            &scratch,
            r#"case "$1 $2" in
  "passwd volant-ldapuser"|"passwd 4242") echo "volant-ldapuser:x:4242:4242::/:/bin/sh" ;;
  "group volant-ldapgroup"|"group 4243") echo "volant-ldapgroup:x:4243:" ;;
  *) exit 2 ;;
esac"#,
        );
        let clock = unbounded();
        assert!(matches!(
            owner_account("volant-ldapuser", clock),
            Ok(Account::Id(4242))
        ));
        assert_eq!(
            lookup_user("4242", clock).unwrap().unwrap().name,
            "volant-ldapuser"
        );
        assert!(matches!(
            group_account("volant-ldapgroup", clock),
            Ok(Account::Id(4243))
        ));
        assert_eq!(
            lookup_group("4243", clock).unwrap().unwrap().name,
            "volant-ldapgroup"
        );
        assert!(matches!(
            owner_account("volant-nobody", clock),
            Ok(Account::Unknown(_))
        ));
    }

    /// Python's `str()` of an `OSError` raised on a bytes path, glibc's wording.
    ///
    /// What would make this red: the quote not switched for a path holding `'`, or a byte
    /// outside ASCII printed as a character rather than `\xNN`.
    #[test]
    fn an_os_error_reads_as_python_prints_it() {
        let denied = io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(
            os_error(&denied, "/x'y"),
            "[Errno 13] Permission denied: b\"/x'y\""
        );
        assert_eq!(
            os_error(&denied, "/é\n"),
            "[Errno 13] Permission denied: b'/\\xc3\\xa9\\n'"
        );
    }

    /// Owner, then group, then mode, and the first failure stops the rest.
    ///
    /// What would make this red: the mode applied before the group (the file would be 0600 after
    /// the group's failure), or a group lookup failure reported as anything but the reference's.
    #[test]
    #[cfg(target_os = "linux")]
    fn attributes_are_set_in_order_and_stop_at_the_first_failure() {
        let scratch = golden::Scratch::new("attrs");
        let file = scratch.fixture();
        let me = Account::Id(unsafe { libc::geteuid() });
        let err = set_fs_attributes(
            &file,
            Some(&me),
            Some(&Account::Unknown("volant-no-such-group".into())),
            Some(&json!("0600")),
        )
        .unwrap_err();
        let FsError::Failed(fail) = err else {
            panic!("{err:?}")
        };
        assert_eq!(
            fail["msg"],
            "chgrp failed: failed to look up group volant-no-such-group"
        );
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o7777, 0o644);
        assert!(set_fs_attributes(&file, None, None, Some(&json!("0600"))).unwrap());
        assert!(!set_fs_attributes(&file, None, None, Some(&json!(384))).unwrap());
    }
}

/// What the natives' tests compare against: the recordings of ansible-core 2.19.12 under
/// `crates/volant/tests/golden/native/`, taken on a scratch directory like the one the golden
/// test builds.
#[cfg(all(test, target_os = "linux"))]
pub mod golden {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use serde_json::{Map, Value};

    /// A directory under `/var/tmp` (the recordings' file system is not `tmpfs`), removed when
    /// dropped.
    pub struct Scratch(pub String);

    impl Scratch {
        pub fn new(name: &str) -> Self {
            let dir = format!("/var/tmp/volant-native-{name}-{}", std::process::id());
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            Self(dir)
        }

        /// `f.txt` holding `hello\n` in 0644 and `l`, a link to it, as the golden play sets
        /// them up. Returns the file's path.
        pub fn fixture(&self) -> String {
            let file = self.path("f.txt");
            fs::write(&file, "hello\n").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
            std::os::unix::fs::symlink("f.txt", self.path("l")).unwrap();
            file
        }

        pub fn path(&self, name: &str) -> String {
            format!("{}/{name}", self.0)
        }
    }

    /// A `getent` running `body` (`sh`), put first on this test process's `PATH`. Nextest runs
    /// each test in a process of its own, so no other test sees it.
    pub fn fake_getent(scratch: &Scratch, body: &str) {
        let bin = scratch.path("bin");
        fs::create_dir(&bin).unwrap();
        let getent = format!("{bin}/getent");
        fs::write(&getent, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&getent, fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        unsafe { std::env::set_var("PATH", format!("{bin}:{path}")) };
    }

    /// `f`'s answer, run on a thread, or a failure after `secs`: a native that ignores its clock
    /// would otherwise hold the test until nextest ends it, ten minutes on.
    pub fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (sent, got) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sent.send(f());
        });
        got.recv_timeout(std::time::Duration::from_secs(secs))
            .expect("the native did not stop at the task's timeout or cancel")
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Compares a native's answer with a recording, key by key, and returns every difference.
    ///
    /// The recording loses what the controller adds (`action`, `_ansible_no_log`, `exception`)
    /// and the golden test's own `_after`. The answer gets the `changed` the dispatcher fills in,
    /// the scratch directory written `<golden-tmp>`, and the running account written as the
    /// recording's placeholders. A `volatile` key (`stat.atime`) is only required on both sides.
    pub fn differences(
        recording: &str,
        answer: Map<String, Value>,
        scratch: &Scratch,
        volatile: &[&str],
    ) -> Vec<String> {
        let mut want: Map<String, Value> = serde_json::from_str(recording).unwrap();
        for key in ["action", "_ansible_no_log", "exception", "_after"] {
            want.remove(key);
        }
        let mut ours = crate::modules::module_result(answer).0;
        mask(&mut ours, scratch);
        let mut found = Vec::new();
        compare(
            "",
            &Value::Object(want),
            &Value::Object(ours),
            volatile,
            &mut found,
        );
        found
    }

    fn mask(map: &mut Map<String, Value>, scratch: &Scratch) {
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let clock = super::super::setup::unbounded();
        let user = super::lookup_user(&uid.to_string(), clock)
            .unwrap()
            .unwrap()
            .name;
        let group = super::lookup_group(&gid.to_string(), clock)
            .unwrap()
            .unwrap()
            .name;
        for (key, value) in map.iter_mut() {
            let placeholder = match (key.as_str(), &*value) {
                ("uid", v) if *v == uid => Some("<uid>"),
                ("gid", v) if *v == gid => Some("<gid>"),
                ("owner" | "pw_name", v) if *v == user.as_str() => Some("<user>"),
                ("group" | "gr_name", v) if *v == group.as_str() => Some("<group>"),
                _ => None,
            };
            if let Some(placeholder) = placeholder {
                *value = placeholder.into();
                continue;
            }
            mask_value(value, scratch);
        }
    }

    /// The scratch directory written `<golden-tmp>` in a value, inside lists (`lineinfile`'s
    /// `diff`) as well as maps.
    fn mask_value(value: &mut Value, scratch: &Scratch) {
        match value {
            Value::String(text) => *text = text.replace(&scratch.0, "<golden-tmp>"),
            Value::Object(inner) => mask(inner, scratch),
            Value::Array(items) => {
                for item in items {
                    mask_value(item, scratch);
                }
            }
            _ => {}
        }
    }

    fn compare(at: &str, want: &Value, ours: &Value, volatile: &[&str], found: &mut Vec<String>) {
        if let (Value::Object(want), Value::Object(ours)) = (want, ours) {
            for key in want
                .keys()
                .chain(ours.keys().filter(|k| !want.contains_key(*k)))
            {
                let path = if at.is_empty() {
                    key.clone()
                } else {
                    format!("{at}.{key}")
                };
                match (want.get(key), ours.get(key)) {
                    (Some(w), Some(o)) if !volatile.contains(&path.as_str()) => {
                        compare(&path, w, o, volatile, found);
                    }
                    (Some(_), Some(_)) => {}
                    (w, o) => found.push(format!("{path}: reference {w:?}, ours {o:?}")),
                }
            }
        } else if want != ours {
            found.push(format!("{at}: reference {want}, ours {ours}"));
        }
    }
}
