// SPDX-License-Identifier: GPL-3.0-or-later
//! `lineinfile`, answered in the agent as `ansible/modules/lineinfile.py` of ansible-core 2.19.12
//! answers it.
//!
//! The native reads the file and works out its new lines in memory first, with the reference's
//! own rules: the file read as bytes and split after each `\n`, `regexp` searched line by line
//! with the last match winning unless `firstmatch`, `insertafter`/`insertbefore` read the same
//! way, `search_string` as a plain substring. Only then does it act, in the reference's order:
//! parent directories, backup, temporary file, `validate`, rename, owner, group and mode. Every
//! hand-back comes before the first of those. Outside the subset the Python module answers:
//! `backrefs`, an expression Rust's `regex` has no exact counterpart for, SELinux, `attributes`,
//! a link or a special file as the path, extended attributes on the file, a task `environment`.

use std::collections::BTreeMap;
#[cfg(all(test, target_os = "linux"))]
use std::ffi::CString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use regex::bytes::{Regex, RegexBuilder};
use serde_json::{Map, Value, json};

use super::common::{
    APPEND, AUTOMATIC, Account, ArgSpec, BackupError, Clock, FsError, IMMUTABLE, ModeError,
    Stop as Halt, access, add_path_info, backup_local, bool_param, check, check_names,
    chown_if_permitted, clock, flag, group_account, has_flags, has_xattrs, module_args, native_run,
    null, os_error, owner_account, parent, parse_mode, path_param, selinux_enabled,
    set_fs_attributes_diff, str_param, umask, validate_runs_as_python,
};
use super::setup::run_output;
use super::{Native, NativeRun};
use crate::modules::Context;

pub const NATIVE: Native = Native {
    name: "lineinfile",
    aliases: &[],
    enabled: true,
    run,
};

/// `lineinfile`'s own options, then `add_file_common_args`.
const SPEC: &[ArgSpec] = &[
    ArgSpec {
        name: "path",
        aliases: &["dest", "destfile", "name"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "state",
        aliases: &[],
        default: || json!("present"),
    },
    ArgSpec {
        name: "regexp",
        aliases: &["regex"],
        default: || Value::Null,
    },
    null("search_string"),
    ArgSpec {
        name: "line",
        aliases: &["value"],
        default: || Value::Null,
    },
    null("insertafter"),
    null("insertbefore"),
    flag("backrefs", false),
    flag("create", false),
    flag("backup", false),
    flag("firstmatch", false),
    null("validate"),
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

/// The reference's `mutually_exclusive`, counted by presence as `AnsibleModule` counts it.
const EXCLUSIVE: &[[&str; 2]] = &[
    ["insertbefore", "insertafter"],
    ["regexp", "search_string"],
    ["backrefs", "search_string"],
];

/// Options whose every value other than `null` is outside the native.
const UNSUPPORTED: &[&str] = &["attributes", "seuser", "serole", "selevel", "setype"];

/// Why the answer stopped short of a result.
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

impl From<&str> for Stop {
    fn from(reason: &str) -> Self {
        Stop::Back(reason.to_string())
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

/// What the task asks for, read and checked before anything is touched.
struct Request<'a> {
    path: &'a str,
    present: bool,
    regexp: Option<Regex>,
    search_string: Option<&'a [u8]>,
    line: Option<&'a str>,
    insertafter: Option<&'a str>,
    insertbefore: Option<&'a str>,
    create: bool,
    backup: bool,
    firstmatch: bool,
    validate: Option<&'a str>,
    owner: Option<Account>,
    group: Option<Account>,
    mode: Option<Value>,
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
    check(clock)?;
    check_names(SPEC, args)?;
    for pair in EXCLUSIVE {
        if pair.iter().all(|name| given(args, name)) {
            return Err(format!("{} and {} are mutually exclusive", pair[0], pair[1]).into());
        }
    }
    let params = module_args(SPEC, args);
    let request = request(&params, clock)?;
    let mut result = match edit(&request, context, clock) {
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

/// Whether `name` was given under any of its spellings.
fn given(args: &Map<String, Value>, name: &str) -> bool {
    SPEC.iter()
        .filter(|arg| arg.name == name)
        .flat_map(|arg| std::iter::once(arg.name).chain(arg.aliases.iter().copied()))
        .any(|spelling| args.contains_key(spelling))
}

/// The parameters as `AnsibleModule` would validate them. Whatever it would convert, warn about
/// or refuse goes back to the Python module, which says it in its own words.
fn request<'a>(params: &'a Map<String, Value>, clock: Clock) -> Result<Request<'a>, Halt> {
    if let Some(name) = UNSUPPORTED.iter().find(|name| !params[**name].is_null()) {
        return Err(format!("{name} is set").into());
    }
    for name in [
        "backrefs",
        "create",
        "backup",
        "firstmatch",
        "unsafe_writes",
    ] {
        bool_param(params, name)?;
    }
    if bool_param(params, "unsafe_writes")? {
        return Err("unsafe_writes is set".into());
    }
    let present = match str_param(params, "state")? {
        Some("present") => true,
        Some("absent") => false,
        _ => return Err("state is not one of its choices".into()),
    };
    let text = |name| str_param(params, name);
    let (regexp, search_string, line) = (text("regexp")?, text("search_string")?, text("line")?);
    let (insertafter, insertbefore, validate) = (
        text("insertafter")?,
        text("insertbefore")?,
        // `valid = not validate`: an empty one is no `validate` at all.
        text("validate")?.filter(|validate| !validate.is_empty()),
    );
    let path = path_param(params, "path")?;
    let mode = Some(params["mode"].clone()).filter(|mode| !mode.is_null());
    if let Some(mode) = &mode
        && let Err(ModeError::Unsupported(why)) = parse_mode(mode, 0, false)
    {
        return Err(why.into());
    }
    // The reference warns about an empty `regexp` or `search_string`; an empty `insertafter` or
    // `insertbefore` is compiled as an expression yet read as false.
    for (name, value) in [
        ("regexp", regexp),
        ("search_string", search_string),
        ("insertafter", insertafter),
        ("insertbefore", insertbefore),
    ] {
        if value == Some("") {
            return Err(format!("{name} is empty").into());
        }
    }
    if bool_param(params, "backrefs")? {
        return Err("backrefs is set".into());
    }
    Ok(Request {
        path,
        present,
        regexp: regexp
            .map(|pattern| compile("regexp", pattern))
            .transpose()?,
        search_string: search_string.map(str::as_bytes),
        line,
        insertafter,
        insertbefore,
        create: bool_param(params, "create")?,
        backup: bool_param(params, "backup")?,
        firstmatch: bool_param(params, "firstmatch")?,
        validate,
        owner: text("owner")?
            .map(|owner| owner_account(owner, clock))
            .transpose()?,
        group: text("group")?
            .map(|group| group_account(group, clock))
            .transpose()?,
        mode,
    })
}

/// `main`, then `present` or `absent`, for a request inside the subset.
#[expect(clippy::too_many_lines)]
fn edit(request: &Request, context: &Context, clock: Clock) -> Result<Map<String, Value>, Stop> {
    let path = request.path;
    if fs::metadata(path).is_ok_and(|meta| meta.is_dir()) {
        return Err(fail(format!("Path {path} is a directory !"), Some(256)));
    }
    if request.present && request.line.is_none() {
        return Err(fail("line is required with state=present".into(), None));
    }
    if !request.present
        && request.regexp.is_none()
        && request.search_string.is_none()
        && request.line.is_none()
    {
        return Err(fail(
            "one of line, search_string, or regexp is required with state=absent".into(),
            None,
        ));
    }
    let exists = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => return Err("the path is a link".into()),
        Ok(meta) if !meta.is_file() => return Err("the path is not a regular file".into()),
        Ok(_) => true,
        Err(err) if err.raw_os_error() == Some(libc::ENOENT) => false,
        Err(err) => return Err(format!("{path}: {err}").into()),
    };
    if !exists {
        if !request.present {
            let mut result = Map::new();
            result.insert("changed".into(), Value::Bool(false));
            result.insert("msg".into(), "file not present".into());
            return Ok(result);
        }
        if !request.create {
            return Err(fail(
                format!("Destination {path} does not exist !"),
                Some(257),
            ));
        }
    }
    let mut lines = if exists {
        readlines(&fs::read(path).map_err(|err| format!("reading {path}: {err}"))?)
    } else {
        Vec::new()
    };

    let mut result = Map::new();
    let (mut changed, mut msg) = if request.present {
        present(request, &mut lines, clock)?
    } else {
        let found = absent(request, &mut lines, clock)?;
        result.insert("found".into(), found.into());
        let msg = if found > 0 {
            format!("{found} line(s) removed")
        } else {
            String::new()
        };
        (found > 0, msg)
    };

    let mut backup = String::new();
    if changed {
        preflight(request, exists, &context.remote_tmp)?;
        check(clock)?;
        if !exists {
            makedirs(path)?;
        }
        if request.backup && exists {
            backup = match backup_local(path, clock) {
                Ok(name) => name,
                Err(BackupError::Exists(name)) => return Err(format!("{name} exists").into()),
                Err(BackupError::Failed(msg)) => return Err(fail(msg, None)),
                Err(BackupError::Stopped(halt)) => return Err(Stop::Clock(halt)),
            };
        }
        write_changes(request, &lines.concat(), &context.remote_tmp, clock)?;
    }

    let mut attr_diff = Map::new();
    match set_fs_attributes_diff(
        path,
        request.owner.as_ref(),
        request.group.as_ref(),
        request.mode.as_ref(),
        &mut attr_diff,
    ) {
        Ok(true) => {
            if changed {
                msg.push_str(" and ");
            }
            changed = true;
            msg.push_str("ownership, perms or SE linux context changed");
        }
        Ok(false) => {}
        Err(FsError::Failed(fail)) => return Err(Stop::Fail(fail)),
        Err(FsError::Raised(error, touched)) if changed || touched => {
            return Err(Stop::Fail(message(error)));
        }
        Err(FsError::Raised(error, _)) => return Err(Stop::Back(error)),
    }
    attr_diff.insert(
        "before_header".into(),
        format!("{path} (file attributes)").into(),
    );
    attr_diff.insert(
        "after_header".into(),
        format!("{path} (file attributes)").into(),
    );
    // Volant refuses `--diff`, so the content half is always the reference's empty one.
    let diff = json!({
        "before": "",
        "after": "",
        "before_header": format!("{path} (content)"),
        "after_header": format!("{path} (content)"),
    });
    result.insert("changed".into(), Value::Bool(changed));
    result.insert("msg".into(), msg.into());
    result.insert("backup".into(), backup.into());
    result.insert("diff".into(), json!([diff, attr_diff]));
    Ok(result)
}

/// `f.readlines()` on a file opened `rb`: each line keeps its `\n`, the last may have none.
fn readlines(content: &[u8]) -> Vec<Vec<u8>> {
    content
        .split_inclusive(|&byte| byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

/// `b_cur_line.rstrip(b'\r\n')`.
fn rstrip(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .rposition(|&byte| byte != b'\r' && byte != b'\n')
        .map_or(0, |at| at + 1);
    &line[..end]
}

/// Stops a scan of a large file at the task's `timeout` or cancel.
fn tick(lineno: usize, clock: Clock) -> Result<(), Halt> {
    if lineno.is_multiple_of(4096) {
        check(clock)?;
    }
    Ok(())
}

/// `present`: the new lines, whether they changed, and the message.
#[expect(clippy::too_many_lines)]
fn present(
    request: &Request,
    lines: &mut Vec<Vec<u8>>,
    clock: Clock,
) -> Result<(bool, String), Halt> {
    let line = request.line.unwrap_or_default().as_bytes();
    // `main`'s own default, set there rather than in the spec because of `mutually_exclusive`.
    let insertafter = match (request.insertafter, request.insertbefore) {
        (None, None) => Some("EOF"),
        (after, _) => after,
    };
    let insertbefore = request.insertbefore;
    let ins = match (insertafter, insertbefore) {
        (Some(after), _) if !matches!(after, "BOF" | "EOF") => Some(compile("insertafter", after)?),
        (_, Some(before)) if before != "BOF" => Some(compile("insertbefore", before)?),
        _ => None,
    };
    let firstmatch = request.firstmatch;

    // Where the expression or the string matched (`index[0]`), where to insert (`index[1]`).
    let mut found: Option<usize> = None;
    let mut insert_at: Option<usize> = None;
    let mut matched = false;
    if let Some(regexp) = &request.regexp {
        for (lineno, cur) in lines.iter().enumerate() {
            tick(lineno, clock)?;
            if regexp.is_match(cur) {
                found = Some(lineno);
                matched = true;
                if firstmatch {
                    break;
                }
            }
        }
    }
    if let Some(search) = request.search_string {
        for (lineno, cur) in lines.iter().enumerate() {
            tick(lineno, clock)?;
            if cur.windows(search.len()).any(|window| window == search) {
                found = Some(lineno);
                matched = true;
                if firstmatch {
                    break;
                }
            }
        }
    }
    if !matched {
        for (lineno, cur) in lines.iter().enumerate() {
            tick(lineno, clock)?;
            if line == rstrip(cur) {
                found = Some(lineno);
            } else if ins.as_ref().is_some_and(|ins| ins.is_match(cur)) {
                if insertafter.is_some() {
                    insert_at = Some(lineno + 1);
                    if firstmatch {
                        break;
                    }
                }
                if insertbefore.is_some() {
                    insert_at = Some(lineno);
                    if firstmatch {
                        break;
                    }
                }
            }
        }
    }

    let with_sep = || [line, b"\n"].concat();
    let added = || Ok((true, "line added".to_string()));
    let unchanged = || Ok((false, String::new()));
    // The reference's branch for "no regexp, no search_string, no exact line" under `found`
    // cannot be reached: `found` is only ever set by one of the three.
    if let Some(at) = found {
        let new = if line.ends_with(b"\n") {
            line.to_vec()
        } else {
            with_sep()
        };
        if lines[at] != new {
            lines[at] = new;
            return Ok((true, "line replaced".to_string()));
        }
        return unchanged();
    }
    if insertbefore == Some("BOF") || insertafter == Some("BOF") {
        lines.insert(0, with_sep());
        return added();
    }
    let Some(at) = insert_at.filter(|_| insertafter != Some("EOF")) else {
        if lines
            .last()
            .is_some_and(|last| !matches!(last.last(), Some(b'\n' | b'\r')))
        {
            lines.push(b"\n".to_vec());
        }
        lines.push(with_sep());
        return added();
    };
    if insertafter.is_some() {
        if lines.len() == at {
            if rstrip(&lines[at - 1]) != line {
                lines.push(with_sep());
                return added();
            }
        } else if line != rstrip(&lines[at]) {
            lines.insert(at, with_sep());
            return added();
        }
        return unchanged();
    }
    lines.insert(at, with_sep());
    added()
}

/// `absent`: drops every matching line and returns how many it dropped.
fn absent(request: &Request, lines: &mut Vec<Vec<u8>>, clock: Clock) -> Result<usize, Halt> {
    let line = request.line.unwrap_or_default().as_bytes();
    let mut kept = Vec::with_capacity(lines.len());
    let mut found = 0;
    for (lineno, cur) in lines.drain(..).enumerate() {
        tick(lineno, clock)?;
        let hit = match (&request.regexp, request.search_string) {
            (Some(regexp), _) => regexp.is_match(&cur),
            (None, Some(search)) => cur.windows(search.len()).any(|window| window == search),
            (None, None) => line == rstrip(&cur),
        };
        if hit {
            found += 1;
        } else {
            kept.push(cur);
        }
    }
    *lines = kept;
    Ok(found)
}

/// What would hand the write back, checked before its first change.
fn preflight(request: &Request, exists: bool, tmpdir: &str) -> Result<(), String> {
    if !fs::metadata(tmpdir).is_ok_and(|meta| meta.is_dir()) {
        return Err(format!("remote_tmp {tmpdir} is not a directory"));
    }
    // The backup and the rename would fail there, with an exception the Python module words.
    let dir = parent(request.path);
    if fs::metadata(dir).is_ok() && !access(dir, libc::W_OK | libc::X_OK) {
        return Err(format!("{dir} is not writable"));
    }
    // `shutil.copystat` copies them, to the backup and to the new file.
    if exists && has_xattrs(request.path) {
        return Err("the file has extended attributes".into());
    }
    // The rename fails on an immutable or append-only file, with an exception the Python
    // module words; `backup_local` copies the flags `lsattr` shows with `chattr`.
    if exists && has_flags(request.path, IMMUTABLE | APPEND) {
        return Err("the file is immutable or append-only".into());
    }
    if exists && request.backup && has_flags(request.path, !AUTOMATIC) {
        return Err("the file has inode flags".into());
    }
    let Some(validate) = request.validate.filter(|validate| validate.contains("%s")) else {
        return Ok(());
    };
    validate_runs_as_python(validate, &format!("{tmpdir}/tmpxxxxxxxx"), "remote_tmp")
}

/// `os.makedirs(os.path.dirname(dest))` for a file `create` makes. A first directory that
/// cannot be made hands back, since nothing changed yet; a later one fails as the reference does.
fn makedirs(path: &str) -> Result<(), Stop> {
    let parent = parent(path);
    if parent.is_empty() || fs::metadata(parent).is_ok() {
        return Ok(());
    }
    let mut made = false;
    let mut current = String::new();
    for name in parent.trim_start_matches('/').split('/') {
        current = format!("{current}/{name}");
        if fs::metadata(&current).is_ok() {
            continue;
        }
        match fs::create_dir(&current) {
            Ok(()) => made = true,
            Err(err) if made => {
                return Err(fail(
                    format!("Error creating {parent} ({})", os_error(&err, &current)),
                    None,
                ));
            }
            Err(err) => return Err(Stop::Back(os_error(&err, &current))),
        }
    }
    Ok(())
}

/// A file created as `tempfile.mkstemp` creates one: `0600`, a name nobody else has.
fn mkstemp(dir: &str, prefix: &str, suffix: &str) -> io::Result<(String, fs::File)> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    for attempt in 0..100_u32 {
        let name = format!(
            "{dir}/{prefix}{:08x}{suffix}",
            seed ^ std::process::id().rotate_left(16) ^ attempt.wrapping_mul(0x9e37_79b9)
        );
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&name)
        {
            Ok(file) => return Ok((name, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

/// A temporary file of the native's, gone when dropped unless it was moved into place.
struct Temp(String);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// `write_changes`: the content to a temporary file under `remote_tmp`, `validate` run on it,
/// then `atomic_move` over the path. A failing `validate` leaves the path as it was.
fn write_changes(
    request: &Request,
    content: &[u8],
    tmpdir: &str,
    clock: Clock,
) -> Result<(), Stop> {
    let path = request.path;
    let raised = |err: io::Error, at: &str| fail(os_error(&err, at), None);
    let (name, mut file) = mkstemp(tmpdir, "tmp", "").map_err(|err| raised(err, tmpdir))?;
    let tmp = Temp(name);
    file.write_all(content).map_err(|err| raised(err, &tmp.0))?;
    drop(file);
    if let Some(validate) = request.validate {
        if !validate.contains("%s") {
            return Err(fail(format!("validate must contain %s: {validate}"), None));
        }
        let argv = shlex::split(&validate.replace("%s", &tmp.0)).unwrap_or_default();
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| fail("validate names no program".into(), None))?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match run_output(&BTreeMap::new(), clock, Path::new(program), &args)? {
            Ok((0, _, _)) => {}
            Ok((rc, _, err)) => {
                return Err(fail(
                    format!("failed to validate: rc:{rc} error:{err}"),
                    None,
                ));
            }
            // Checked up front; only a program removed since then gets here. `run_command`'s
            // answer to the `OSError`, its `cmd` without `_clean_args`' masking.
            Err(errno) => {
                let mut fail = message("Error executing command.".into());
                fail.insert("rc".into(), errno.into());
                fail.insert("stdout".into(), "".into());
                fail.insert("stderr".into(), "".into());
                fail.insert("cmd".into(), argv.join(" ").into());
                return Err(Stop::Fail(fail));
            }
        }
    }
    atomic_move(&tmp.0, path).map_err(|err| fail(err, None))
}

/// `atomic_move(src, realpath(dest))`, `keep_dest_attrs` and no `unsafe_writes`: the
/// destination's owner and mode given to `src`, a rename, and the same done through a
/// temporary file beside the destination when the rename cannot cross to it. A file created
/// gets `0666` less the umask, and the running account.
fn atomic_move(src: &str, dest: &str) -> Result<(), String> {
    let dest_stat = fs::metadata(dest).ok();
    if let Some(stat) = &dest_stat {
        // A refused `chown` skips `copystat` as well: both sit in one `try`.
        match std::os::unix::fs::chown(src, Some(stat.uid()), Some(stat.gid())) {
            Ok(()) => fs::set_permissions(src, fs::Permissions::from_mode(stat.mode() & 0o7777))
                .map_err(|err| os_error(&err, src))?,
            Err(err) if err.raw_os_error() == Some(libc::EPERM) => {}
            Err(err) => return Err(os_error(&err, src)),
        }
    }
    if let Err(err) = fs::rename(src, dest) {
        let workaround = matches!(
            err.raw_os_error(),
            Some(libc::EPERM | libc::EXDEV | libc::EACCES | libc::ETXTBSY | libc::EBUSY)
        );
        if !workaround {
            return Err(format!("Could not replace '{dest}' with '{src}'."));
        }
        let dir = parent(dest);
        let base = dest.rsplit('/').next().unwrap_or(dest);
        let (name, file) = mkstemp(dir, ".ansible_tmp", base).map_err(|_| {
            format!("The destination directory '{dir}' is not writable by the current user.")
        })?;
        drop(file);
        let beside = Temp(name);
        fs::copy(src, &beside.0)
            .map_err(|_| format!("Failed to replace '{dest}' with '{src}'."))?;
        if let Some(stat) = &dest_stat {
            chown_if_permitted(&beside.0, stat)
                .map_err(|_| format!("Failed to replace '{dest}' with '{src}'."))?;
        }
        fs::rename(&beside.0, dest).map_err(|_| {
            format!(
                "Unable to make '{src}' into to '{dest}', failed final rename from '{}'.",
                beside.0
            )
        })?;
    }
    if dest_stat.is_none() {
        let umask = umask().map_err(|_| "the umask cannot be read".to_string())?;
        fs::set_permissions(dest, fs::Permissions::from_mode(0o666 & !umask))
            .map_err(|err| os_error(&err, dest))?;
        let dir = fs::metadata(parent(dest)).map_err(|err| os_error(&err, dest))?;
        // SAFETY: plain getters.
        let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let gid = if dir.mode() & 0o2000 != 0 {
            dir.gid()
        } else {
            egid
        };
        let _ = std::os::unix::fs::chown(dest, Some(euid), Some(gid));
    }
    Ok(())
}

/// A `fail_json(msg=msg)`, with `rc` when the reference gives one.
fn fail(msg: String, rc: Option<i32>) -> Stop {
    let mut fail = message(msg);
    if let Some(rc) = rc {
        fail.insert("rc".into(), rc.into());
    }
    Stop::Fail(fail)
}

fn message(msg: String) -> Map<String, Value> {
    let mut fail = Map::new();
    fail.insert("msg".into(), msg.into());
    fail
}

/// `re.compile(to_bytes(pattern))`, as a Rust expression that matches exactly the lines the
/// Python one would find with `search`, or the reason there is none. The reason names the
/// argument `name` and never quotes the pattern, which can carry a secret.
fn compile(name: &str, pattern: &str) -> Result<Regex, String> {
    let rust = translate(pattern).map_err(|why| format!("{name} holds {why}"))?;
    RegexBuilder::new(&rust)
        .unicode(false)
        .build()
        .map_err(|_| format!("{name} is an expression the Rust engine refuses"))
}

/// What the last item of the branch being read is, for Python's repeat rules.
#[derive(PartialEq)]
enum Last {
    /// Nothing yet: the start, `(` or `|`.
    Nothing,
    /// `^`, `$`, `\A`, `\b` or `\B`, which Python refuses to repeat.
    Assertion,
    /// Something a repeat applies to.
    Item,
    /// A repeat, which Python refuses to repeat again.
    Repeat,
}

/// A Python `re` pattern compiled from bytes, rewritten for Rust's `regex` with Unicode off.
///
/// Compiled from bytes, Python's pattern reads every byte as one character: a non-ASCII
/// character of the text is its UTF-8 bytes, one literal each, in a set as well. `\d`, `\w`,
/// `\s` and `\b` are ASCII, and `.` is any byte but `\n`, which is what Rust's are with Unicode
/// off. `$` also matches before a final `\n`, and every line but the last ends with one, so it
/// becomes `(?m:$)`, which matches there and at the end, and nowhere else in a line.
///
/// Every literal is written `\xHH` or as the ASCII letter or digit it is, so no Rust-only
/// syntax (`\<`, `[[:alpha:]]`, `&&`) can creep in through one. What Python reads and Rust has
/// no counterpart for (lookaround, `(?P=name)`, `\Z`, back references, inline flags,
/// possessive repeats), and what Python refuses (a repeat of nothing or of a repeat, an
/// unknown escape), is an `Err`, and the task goes to the Python module.
#[expect(clippy::too_many_lines)]
fn translate(pattern: &str) -> Result<String, String> {
    let bytes = pattern.as_bytes();
    let mut out = String::new();
    let mut last = Last::Nothing;
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        at += 1;
        match byte {
            b'\\' => {
                let (item, next) = escape(bytes, at, false)?;
                at = next;
                last = if matches!(item, Item::Assertion(_)) {
                    Last::Assertion
                } else {
                    Last::Item
                };
                item.write(&mut out);
            }
            b'(' => {
                if bytes.get(at) == Some(&b'?') {
                    let rest = &bytes[at..];
                    if rest.starts_with(b"?:") {
                        out.push_str("(?:");
                        at += 2;
                    } else if rest.starts_with(b"?P<") {
                        let end = rest
                            .iter()
                            .position(|&b| b == b'>')
                            .ok_or("an unterminated group name")?;
                        let name = &rest[3..end];
                        let identifier = name.first().is_some_and(|b| !b.is_ascii_digit())
                            && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_');
                        if !identifier {
                            return Err("a group name Python refuses".into());
                        }
                        out.push_str("(?P<");
                        out.push_str(std::str::from_utf8(name).unwrap_or_default());
                        out.push('>');
                        at += end + 1;
                    } else {
                        return Err("a (? group other than (?: and (?P<name>".into());
                    }
                } else {
                    out.push('(');
                }
                last = Last::Nothing;
            }
            b')' => {
                out.push(')');
                last = Last::Item;
            }
            b'|' => {
                out.push('|');
                last = Last::Nothing;
            }
            b'^' => {
                out.push('^');
                last = Last::Assertion;
            }
            b'$' => {
                out.push_str("(?m:$)");
                last = Last::Assertion;
            }
            b'.' => {
                out.push('.');
                last = Last::Item;
            }
            b'[' => {
                at = set(bytes, at, &mut out)?;
                last = Last::Item;
            }
            b'*' | b'+' | b'?' | b'{' => {
                let repeat = if byte == b'{' {
                    let Some((repeat, next)) = braces(bytes, at) else {
                        // Not a repeat: Python reads the brace as itself.
                        Item::Byte(byte).write(&mut out);
                        last = Last::Item;
                        continue;
                    };
                    at = next;
                    repeat
                } else {
                    char::from(byte).to_string()
                };
                if last != Last::Item {
                    return Err("a repeat of what Python refuses to repeat".into());
                }
                out.push_str(&repeat);
                match bytes.get(at) {
                    Some(b'?') => {
                        out.push('?');
                        at += 1;
                    }
                    Some(b'+') => return Err("a possessive repeat".into()),
                    _ => {}
                }
                last = Last::Repeat;
            }
            _ => {
                Item::Byte(byte).write(&mut out);
                last = Last::Item;
            }
        }
    }
    Ok(out)
}

/// One element of a pattern after a backslash, or of a set.
enum Item {
    Byte(u8),
    /// `d`, `D`, `s`, `S`, `w` or `W`.
    Class(u8),
    /// `A`, `b` or `B` outside a set.
    Assertion(u8),
}

impl Item {
    fn write(&self, out: &mut String) {
        match self {
            Item::Byte(byte) if byte.is_ascii_alphanumeric() => out.push(char::from(*byte)),
            Item::Byte(byte) => {
                let _ = write!(out, "\\x{byte:02X}");
            }
            Item::Class(letter) | Item::Assertion(letter) => {
                out.push('\\');
                out.push(char::from(*letter));
            }
        }
    }
}

/// The escape starting at `bytes[at]`, just after its backslash (`_escape`, or `_class_escape`
/// inside a set), and where it ends.
fn escape(bytes: &[u8], at: usize, in_set: bool) -> Result<(Item, usize), String> {
    let &letter = bytes.get(at).ok_or("a trailing backslash")?;
    let item = match letter {
        b'd' | b'D' | b's' | b'S' | b'w' | b'W' => Item::Class(letter),
        b'b' if in_set => Item::Byte(0x08),
        b'A' | b'b' | b'B' if !in_set => Item::Assertion(letter),
        b'a' => Item::Byte(0x07),
        b'f' => Item::Byte(0x0c),
        b'n' => Item::Byte(b'\n'),
        b'r' => Item::Byte(b'\r'),
        b't' => Item::Byte(b'\t'),
        b'v' => Item::Byte(0x0b),
        b'x' => {
            let hex = bytes
                .get(at + 1..at + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or("an incomplete \\x escape")?;
            let value = u8::from_str_radix(hex, 16).map_err(|err| err.to_string())?;
            return Ok((Item::Byte(value), at + 3));
        }
        // Back references, octal escapes, `\Z`, and letters Python refuses.
        _ if letter.is_ascii_alphanumeric() => {
            return Err(format!(
                "the escape \\{} with no exact counterpart",
                char::from(letter)
            ));
        }
        _ => Item::Byte(letter),
    };
    Ok((item, at + 1))
}

/// `{m,n}` and its shorter forms, starting just after the brace, as Rust writes them, and where
/// it ends; `None` when Python reads the brace as a literal.
fn braces(bytes: &[u8], mut at: usize) -> Option<(String, usize)> {
    if bytes.get(at) == Some(&b'}') {
        return None;
    }
    let digits = |at: &mut usize| {
        let start = *at;
        while bytes.get(*at).is_some_and(u8::is_ascii_digit) {
            *at += 1;
        }
        String::from_utf8_lossy(&bytes[start..*at]).into_owned()
    };
    let low = digits(&mut at);
    let high = if bytes.get(at) == Some(&b',') {
        at += 1;
        Some(digits(&mut at))
    } else {
        None
    };
    if bytes.get(at) != Some(&b'}') {
        return None;
    }
    let low = if low.is_empty() { "0".to_string() } else { low };
    let repeat = match high {
        None => format!("{{{low}}}"),
        Some(high) => format!("{{{low},{high}}}"),
    };
    Some((repeat, at + 1))
}

/// A set, from just after its `[`, written to `out`; returns where it ends.
fn set(bytes: &[u8], mut at: usize, out: &mut String) -> Result<usize, String> {
    out.push('[');
    if bytes.get(at) == Some(&b'^') {
        out.push('^');
        at += 1;
    }
    let start = at;
    loop {
        let &byte = bytes.get(at).ok_or("an unterminated set")?;
        if byte == b']' && at != start {
            out.push(']');
            return Ok(at + 1);
        }
        // Python warns about what may become nested sets and set operations.
        if byte == b'[' || (b"-&~|".contains(&byte) && bytes.get(at + 1) == Some(&byte)) {
            return Err("a set Python warns about".into());
        }
        let (low, next) = element(bytes, at)?;
        at = next;
        if bytes.get(at) == Some(&b'-') && bytes.get(at + 1).is_some_and(|&b| b != b']') {
            let (high, next) = element(bytes, at + 1)?;
            match (low, high) {
                (Item::Byte(low), Item::Byte(high)) if low <= high => {
                    let _ = write!(out, "\\x{low:02X}-\\x{high:02X}");
                }
                _ => return Err("a bad set range".into()),
            }
            at = next;
        } else {
            low.write(out);
        }
    }
}

/// One element of a set, and where it ends.
fn element(bytes: &[u8], at: usize) -> Result<(Item, usize), String> {
    match bytes[at] {
        b'\\' => escape(bytes, at + 1, true),
        byte => Ok((Item::Byte(byte), at + 1)),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
