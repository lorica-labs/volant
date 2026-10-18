// SPDX-License-Identifier: GPL-3.0-or-later
//! `copy`, answered in the agent as `ansible/modules/copy.py` of ansible-core 2.19.12 answers the
//! `copy` action plugin: `src` is the file the agent staged for the task, and the module moves it
//! over `dest` when the two differ.
//!
//! The native reads first: the source's sums, `checksum`, where `dest` really is, whether it
//! exists and what it holds, and everything that would make the move take the reference's
//! workarounds. Only then does it act, in the reference's order: backup, a link replaced by an
//! empty file, `validate` on the source, then `atomic_move`, which gives the source the owner,
//! mode and a fresh time of the file it replaces and renames it into place. Owner, group and mode
//! come last. Every hand-back comes before the backup. Outside the subset the Python module
//! answers: `remote_src`, `directory_mode`, `content`, SELinux, `attributes`, a destination with
//! extended attributes or an ACL, a source on another mount than the destination (the reference
//! then copies through a `.ansible_tmp*` file), directories the reference would create, and a
//! task `environment`.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use serde_json::{Map, Value, json};

use super::common::{
    APPEND, AUTOMATIC, BackupError, FsError, IMMUTABLE, access, add_path_info, backup_local, check,
    check_names, clock, has_flags, has_xattrs, module_args, native_run, parent, realpath,
    selinux_enabled, set_fs_attributes, validate_runs_as_python,
};
use super::runtime::{Clock, Stop as Halt, run_output};
use super::{Native, NativeRun};
use crate::modules::Context;

mod digest;
mod mount;
mod request;

#[cfg(all(test, target_os = "linux"))]
use digest::Md5;
use digest::{digests, sha1_of};
use mount::{crosses, mount_id};
use request::{Request, SPEC, Stop, request};

pub const NATIVE: Native = Native {
    name: "copy",
    aliases: &[],
    enabled: true,
    run,
};

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
    // `md5sum` is `null` where Python's hashlib refuses MD5.
    if fs::read_to_string("/proc/sys/crypto/fips_enabled").is_ok_and(|on| on.trim() != "0") {
        return Err("the host is in FIPS mode".into());
    }
    check(clock)?;
    check_names(SPEC, args)?;
    let params = module_args(SPEC, args);
    let request = request(&params, clock)?;
    let mut result = match copy(&request, clock) {
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

/// `main` for a request inside the subset.
#[expect(clippy::too_many_lines)]
fn copy(request: &Request, clock: Clock) -> Result<Map<String, Value>, Stop> {
    let src = request.src;
    if !(fs::symlink_metadata(src).is_ok_and(|meta| meta.is_file()) && access(src, libc::R_OK)) {
        return Err("src is not a readable regular file".into());
    }
    let (checksum_src, md5sum) = digests(src, clock)?;
    if let Some(expected) = request.checksum
        && expected != checksum_src
    {
        let mut fail =
            message("Copied file does not match the expected checksum. Transfer failed.".into());
        fail.insert("checksum".into(), checksum_src.into());
        fail.insert("expected_checksum".into(), expected.into());
        return Err(Stop::Fail(fail));
    }

    let mut dest = request.dest.to_string();
    if dest.ends_with('/') {
        if let Some(base) = request.original_basename {
            dest = join(&dest, base);
        }
        if fs::metadata(parent(&dest)).is_err() {
            return Err("the destination's directories would be created".into());
        }
    }
    if fs::metadata(&dest).is_ok_and(|meta| meta.is_dir()) {
        let base = request
            .original_basename
            .unwrap_or_else(|| src.rsplit('/').next().unwrap_or(src));
        dest = join(&dest, base);
    }

    let exists = fs::metadata(&dest).is_ok();
    let mut checksum_dest = None;
    if exists {
        if request.follow && is_link(&dest) {
            dest = realpath(&dest)?;
        }
        if !request.force {
            let mut result = message("file already exists".into());
            result.insert("src".into(), src.into());
            result.insert("dest".into(), dest.into());
            result.insert("changed".into(), Value::Bool(false));
            return Ok(result);
        }
        // The reference goes on without a sum and fails, or replaces a device, on its way.
        if !(access(&dest, libc::R_OK) && fs::metadata(&dest).is_ok_and(|meta| meta.is_file())) {
            return Err("dest is not a readable regular file".into());
        }
        checksum_dest = Some(sha1_of(&dest, clock)?);
    } else {
        let dir = parent(&dest);
        if let Err(err) = fs::metadata(dir) {
            let msg = if err.raw_os_error() == Some(libc::EACCES) {
                format!("Destination directory {dir} is not accessible")
            } else {
                format!("Destination directory {dir} does not exist")
            };
            return Err(Stop::Fail(message(msg)));
        }
    }
    let dir = parent(&dest);
    if !access(dir, libc::W_OK) {
        return Err(Stop::Fail(message(format!(
            "Destination {dir} not writable"
        ))));
    }

    let link = is_link(&dest);
    let mut changed = false;
    let mut backup_file = None;
    if checksum_dest.as_ref() != Some(&checksum_src) || link {
        preflight(request, &dest, exists, link)?;
        check(clock)?;
        if request.backup && exists {
            backup_file = Some(match backup_local(&dest, clock) {
                Ok(name) => name,
                Err(BackupError::Exists(name)) => return Err(format!("{name} exists").into()),
                Err(BackupError::Failed(msg)) => return Err(Stop::Fail(message(msg))),
                Err(BackupError::Stopped(halt)) => return Err(Stop::Clock(halt)),
            });
        }
        let failed = |_| Stop::Fail(message(format!("Failed to copy '{src}' to '{dest}'.")));
        // A link is replaced by a file of its own, not written through.
        if link {
            fs::remove_file(&dest).map_err(failed)?;
            fs::File::create(&dest).map_err(failed)?;
        }
        if let Some(validate) = request.validate {
            validate_src(request, validate, clock)?;
        }
        atomic_move(src, &dest, request.umask).map_err(|msg| Stop::Fail(message(msg)))?;
        changed = true;
    }

    match set_fs_attributes(
        &dest,
        request.owner.as_ref(),
        request.group.as_ref(),
        request.mode.as_ref(),
    ) {
        Ok(attributes) => changed |= attributes,
        Err(FsError::Failed(fail)) => return Err(Stop::Fail(fail)),
        Err(FsError::Raised(error, touched)) if changed || touched => {
            return Err(Stop::Fail(message(error)));
        }
        Err(FsError::Raised(error, _)) => return Err(Stop::Back(error)),
    }
    let mut result = Map::new();
    result.insert("dest".into(), dest.into());
    result.insert("src".into(), src.into());
    result.insert("md5sum".into(), md5sum.into());
    result.insert("checksum".into(), checksum_src.into());
    result.insert("changed".into(), Value::Bool(changed));
    if let Some(backup_file) = backup_file {
        result.insert("backup_file".into(), backup_file.into());
    }
    Ok(result)
}

/// What would send the move down the reference's workarounds, or copy what the native does not,
/// checked before its first change. `exists` is whether `dest` resolves to a file, `link`
/// whether it is a link the native replaces.
fn preflight(request: &Request, dest: &str, exists: bool, link: bool) -> Result<(), String> {
    let dir = parent(dest);
    let dir_meta = fs::metadata(dir).map_err(|err| format!("{dir}: {err}"))?;
    if !dir_meta.is_dir() {
        return Err(format!("{dir} is not a directory"));
    }
    // A rename needs to search the directory too; `access` above only asked for writing.
    if !access(dir, libc::W_OK | libc::X_OK) {
        return Err(format!("{dir} is not writable"));
    }
    // `rename` crosses neither file systems nor mounts, and cannot replace a mount point.
    let mount = mount_id(dir, true).ok_or("the mount of the destination cannot be read")?;
    let at_dir = (dir_meta.dev(), Some(mount));
    let src_meta = fs::symlink_metadata(request.src).map_err(|err| format!("src: {err}"))?;
    if crosses((src_meta.dev(), mount_id(request.src, false)), at_dir) {
        return Err(
            "the staged source is on another file system or mount than the destination".into(),
        );
    }
    let here = fs::symlink_metadata(dest).ok();
    if let Some(meta) = &here
        && crosses((meta.dev(), mount_id(dest, false)), at_dir)
    {
        return Err("the destination is a mount point or a subvolume of its own".into());
    }
    // A sticky directory keeps others' files from being replaced or unlinked.
    // SAFETY: a plain getter.
    let euid = unsafe { libc::geteuid() };
    if dir_meta.mode() & 0o1000 != 0
        && euid != 0
        && dir_meta.uid() != euid
        && here.is_some_and(|meta| meta.uid() != euid)
    {
        return Err("the sticky directory keeps the destination from being replaced".into());
    }
    if has_flags(dir, APPEND) {
        return Err("the directory is append-only".into());
    }
    if exists && !link && has_flags(dest, IMMUTABLE | APPEND) {
        return Err("the destination is immutable or append-only".into());
    }
    // `shutil.copystat` gives the source the destination's extended attributes, and
    // `preserved_copy` gives them to the backup, through a link too.
    if exists && (!link || request.backup) && has_xattrs(dest) {
        return Err("the destination has extended attributes".into());
    }
    // `preserved_copy` gives the backup the flags `lsattr` shows, through a link too.
    if exists && request.backup && has_flags(dest, !AUTOMATIC) {
        return Err("the destination has inode flags the backup would carry".into());
    }
    // The empty file standing for the link takes the directory's default ACL, which
    // `copystat` then gives the source.
    if link && has_xattrs(dir) {
        return Err("the directory has extended attributes".into());
    }
    match request.validate.filter(|validate| validate.contains("%s")) {
        Some(validate) => validate_runs_as_python(validate, request.src, "src"),
        None => Ok(()),
    }
}

/// The source given the task's mode, then owner and group, then `validate` run on it, as the
/// reference does before the move. Anything that stops here comes after the backup: a failure,
/// never a hand-back.
fn validate_src(request: &Request, validate: &str, clock: Clock) -> Result<(), Stop> {
    let src = request.src;
    let attributes = set_fs_attributes(src, None, None, request.mode.as_ref())
        .and_then(|_| set_fs_attributes(src, request.owner.as_ref(), request.group.as_ref(), None));
    match attributes {
        Ok(_) => {}
        Err(FsError::Failed(fail)) => return Err(Stop::Fail(fail)),
        Err(FsError::Raised(error, _)) => return Err(Stop::Fail(message(error))),
    }
    if !validate.contains("%s") {
        return Err(Stop::Fail(message(format!(
            "validate must contain %s: {validate}"
        ))));
    }
    let argv = shlex::split(&validate.replace("%s", src)).unwrap_or_default();
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| Stop::Fail(message("validate names no program".into())))?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ran =
        run_output(&BTreeMap::new(), clock, Path::new(program), &args).map_err(
            |halt| match halt {
                Halt::HandBack(reason) => Stop::Fail(message(reason)),
                other => Stop::Clock(other),
            },
        )?;
    match ran {
        Ok((0, _, _)) => Ok(()),
        Ok((rc, out, err)) => {
            let mut fail = message("failed to validate".into());
            fail.insert("exit_status".into(), rc.into());
            fail.insert("stdout".into(), out.into());
            fail.insert("stderr".into(), err.into());
            Err(Stop::Fail(fail))
        }
        // Checked up front; only a program removed since then gets here. `run_command`'s
        // answer to the `OSError`.
        Err(errno) => {
            let mut fail = message("Error executing command.".into());
            fail.insert("rc".into(), errno.into());
            fail.insert("stdout".into(), "".into());
            fail.insert("stderr".into(), "".into());
            fail.insert("cmd".into(), argv.join(" ").into());
            Err(Stop::Fail(fail))
        }
    }
}

/// `atomic_move(src, dest)` with `keep_dest_attrs`, for a move `preflight` let through: the
/// source given the replaced file's owner, its mode and the time now, then renamed over it. A
/// refused `chown` skips the mode and the time as well, all three sitting in one `try`. A file
/// created gets `0666` less the umask, and the running account.
///
/// The errors are the reference's exception texts. It ends on them with a traceback where this
/// returns them as the failure's message; `preflight` leaves only a race to reach them.
fn atomic_move(src: &str, dest: &str, umask: u32) -> Result<(), String> {
    let failed = |_: io::Error| format!("Failed to copy '{src}' to '{dest}'.");
    if let Ok(stat) = fs::metadata(dest) {
        match std::os::unix::fs::chown(src, Some(stat.uid()), Some(stat.gid())) {
            Ok(()) => {
                fs::set_permissions(src, fs::Permissions::from_mode(stat.mode() & 0o7777))
                    .map_err(failed)?;
                touch(src).map_err(failed)?;
            }
            Err(err) if err.raw_os_error() == Some(libc::EPERM) => {}
            Err(err) => return Err(failed(err)),
        }
    }
    let creating = fs::metadata(dest).is_err();
    fs::rename(src, dest).map_err(|_| format!("Could not replace '{dest}' with '{src}'."))?;
    if creating {
        fs::set_permissions(dest, fs::Permissions::from_mode(0o666 & !umask)).map_err(failed)?;
        let dir = fs::metadata(parent(dest)).map_err(failed)?;
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

/// `os.utime(path, times=(time.time(), time.time()))`.
fn touch(path: &str) -> io::Result<()> {
    let path = CString::new(path).map_err(io::Error::other)?;
    // SAFETY: a null `times` sets both to now.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), std::ptr::null(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn is_link(path: &str) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// `os.path.join(dir, name)`.
fn join(dir: &str, name: &str) -> String {
    if name.starts_with('/') {
        name.to_string()
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn message(msg: String) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("msg".into(), msg.into());
    map
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
