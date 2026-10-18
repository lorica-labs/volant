// SPDX-License-Identifier: GPL-3.0-or-later
//! Writing the file: the temporary file, `validate`, and the rename over the target.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::natives::common::{chown_if_permitted, os_error, parent, umask};
use crate::natives::runtime::{Clock, run_output};

use super::{Request, Stop, fail, message};

/// `os.makedirs(os.path.dirname(dest))` for a file `create` makes. A first directory that
/// cannot be made hands back, since nothing changed yet; a later one fails as the reference does.
pub(super) fn makedirs(path: &str) -> Result<(), Stop> {
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
pub(super) fn write_changes(
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
