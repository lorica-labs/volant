// SPDX-License-Identifier: GPL-3.0-or-later
//! The shared connection of a host: its socket directory, `ssh` options and the file limit.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use super::defaults::SshTarget;

/// How long a shared connection outlives its last `ssh`. Long enough to carry a host from one
/// task to the next and across a barrier; the reference keeps its own for 60 seconds.
pub(super) const CONTROL_PERSIST: &str = "30s";

/// The longest `ControlPath` every platform can bind. `ssh` binds the master's socket under a
/// temporary name first, the path plus `.` and 16 random characters, and renames it after, so
/// the path gets 17 bytes less than `sun_path`, which is 104 bytes with its NUL on macOS and
/// 108 on Linux. Measured with OpenSSH 10.2 on Linux: a 90-byte path binds, a 91-byte one
/// fails with `unix_listener: path "<path>.<16 characters>" too long for Unix domain socket`
/// and exit 255, which would report the host unreachable.
pub(super) const CONTROL_PATH_MAX: usize = 104 - 1 - 17;

/// `ServerAliveInterval` and `ServerAliveCountMax` of a shared connection: a master whose host
/// stopped answering exits after 3 probes 5 seconds apart.
pub(super) const SERVER_ALIVE_INTERVAL: u32 = 5;
pub(super) const SERVER_ALIVE_COUNT_MAX: u32 = 3;

/// The client every connection runs.
pub(super) const SSH_PROGRAM: &str = "ssh";

/// The `ssh -G` keywords that only time a connection and say nothing of where it goes. A
/// socket name must not change with them: `reboot`'s `connect_timeout` changes
/// `connecttimeout` for its reconnection only.
pub(super) const TIMING_ONLY: &[&str] = &[
    "connecttimeout",
    "connectionattempts",
    "serveraliveinterval",
    "serveralivecountmax",
];

/// How long `ssh -O exit` may take. It only talks to a local socket, so this is never reached
/// unless the master itself is wedged.
pub(super) const STOP_MASTER_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the operator's own `ssh` arguments rule connection sharing out: they already say
/// something about it, by option name (in any case, as `ssh` reads them) or by the `-S` and
/// `-M` flags that set the same two options, or they ask for debug output with `-v`.
///
/// Under `-v` a backgrounded master keeps the standard error of the `ssh` that started it
/// (OpenSSH's `control_persist_detach` leaves it open when debugging to stderr), so the first
/// bootstrap `ssh` of the host would only return once the master exits, `ControlPersist` after
/// its last client, and the link's master would hold Volant's own standard error past its exit.
pub(super) fn user_sets_control(common: &[String], extra: &[String]) -> bool {
    common.iter().chain(extra).any(|word| {
        let lower = word.to_ascii_lowercase();
        ["controlmaster", "controlpath", "controlpersist"]
            .iter()
            .any(|name| lower.contains(name))
            || word.starts_with("-S")
            || word == "-M"
            || asks_for_debug(word)
    })
}

/// Whether one word is a cluster of `ssh` flags holding `-v`. The letter of an option that
/// takes a value ends the cluster, since the rest of the word is that value: `-lvolant` is a
/// user name, not a `-v`.
pub(super) fn asks_for_debug(word: &str) -> bool {
    let Some(flags) = word.strip_prefix('-').filter(|f| !f.starts_with('-')) else {
        return false;
    };
    for c in flags.chars() {
        if c == 'v' {
            return true;
        }
        if "BbcDEeFIiJLlmOoPpQRSWw".contains(c) {
            return false;
        }
    }
    false
}

/// The socket of one inventory host's shared connection: `dir` and the first 16 hex characters
/// of a blake3 over the inventory name, the delegate if the link is a delegated one, and every
/// `ssh` option the host connects with. The inventory name is in it because `ssh`'s own `%C`
/// is not enough: fifty inventory aliases of one machine would share one master and run past
/// the server's `MaxSessions`. The options are in it so a connection opened with one key, user
/// or proxy never serves a host asking for another; what `ssh` itself resolves from its
/// configuration is added when the link opens (see [`SshTarget::resolved`]). `None` when the
/// path would be too long to bind.
pub(super) fn control_path_for(
    dir: &Path,
    host: &str,
    delegate: Option<&str>,
    target: &SshTarget,
) -> Option<Box<Path>> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(host.as_bytes());
    if let Some(delegate) = delegate {
        hasher.update(b"\0delegate\0");
        hasher.update(delegate.as_bytes());
    }
    for word in target.ssh_argv("") {
        hasher.update(b"\0");
        hasher.update(word.as_bytes());
    }
    let path = dir.join(&hasher.finalize().to_hex()[..16]);
    (path.as_os_str().len() <= CONTROL_PATH_MAX).then(|| path.into_boxed_path())
}

/// The directory for the shared connections' sockets, for this user (see `control_dir_in`),
/// or `None` after a warning saying why there is none.
#[cfg(unix)]
pub fn control_dir() -> Option<PathBuf> {
    // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    control_dir_in(runtime.as_deref(), Path::new("/tmp"), uid)
        .map_err(|why| eprintln!("[WARNING]: ssh connections are not shared: {why}"))
        .ok()
}

/// OpenSSH for Windows has no connection sharing.
#[cfg(not(unix))]
pub fn control_dir() -> Option<PathBuf> {
    None
}

/// `<runtime>/volant-cm` when `runtime` is a directory `uid` owns and nobody else can write to,
/// `<tmp>/volant-cm-<uid>` otherwise, created at mode 0700. Anybody who can create a socket in
/// it can hand the next `ssh` a connection of their own, so an existing directory is used only
/// if it is a real directory, owned by `uid`, at mode 0700; anything else is refused with the
/// reason. Its parent must not let anybody else rename it away and plant their own: not
/// writable by group or others, unless it is sticky as `/tmp` is.
///
/// A runtime directory whose path holds anything but letters, digits and `/._-` is passed
/// over too: `ssh` reads `-o ControlPath=...` as a configuration line, where a space ends the
/// value and `%` starts a token.
#[cfg(unix)]
pub(super) fn control_dir_in(
    runtime: Option<&Path>,
    tmp: &Path,
    uid: u32,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let plain = |p: &Path| {
        p.to_str().is_some_and(|text| {
            text.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
        })
    };
    let owned = |p: &Path| {
        std::fs::symlink_metadata(p)
            .is_ok_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o022 == 0)
    };
    let dir = if let Some(runtime) = runtime.filter(|r| plain(r) && owned(r)) {
        runtime.join("volant-cm")
    } else {
        let parent = std::fs::metadata(tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        if parent.mode() & 0o022 != 0 && parent.mode() & 0o1000 == 0 {
            return Err(format!(
                "{} is writable by others and not sticky",
                tmp.display()
            ));
        }
        tmp.join(format!("volant-cm-{uid}"))
    };
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o777 != 0o700 {
        return Err(format!(
            "{} is not a directory of mode 0700 owned by uid {uid}",
            dir.display()
        ));
    }
    Ok(dir)
}

/// Raises this process's soft limit on open files to its hard limit, capped at 2^20. Every
/// link costs the controller three descriptors, and a soft limit of 1024 left hosts
/// unreachable with `Too many open files` on a run of a few hundred. Nothing is lowered, and
/// a limit the system refuses to raise stays where it was.
#[cfg(unix)]
pub fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes into the struct it is given, which lives for the whole call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    let wanted = limit.rlim_max.min(1 << 20);
    if limit.rlim_cur < wanted {
        limit.rlim_cur = wanted;
        // SAFETY: as above; the struct is only read. A refusal leaves the limit unchanged.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    }
}

#[cfg(not(unix))]
pub fn raise_open_file_limit() {}

/// Whether an `ssh -G` dump shows connection sharing already set up. Measured with OpenSSH
/// 10.2: nothing configured prints `controlmaster false` and `controlpersist no` and no
/// `controlpath` line at all.
pub(super) fn config_sets_control(dump: &str) -> bool {
    dump.lines().any(|line| match line.split_once(' ') {
        Some(("controlpath", _)) => true,
        Some(("controlmaster", value)) => value != "false",
        Some(("controlpersist", value)) => value != "no",
        _ => false,
    })
}

/// The machine an `ssh -G` dump says the connection goes to.
pub(super) fn dump_hostname(dump: &str) -> Option<String> {
    dump.lines()
        .find_map(|line| line.strip_prefix("hostname "))
        .map(str::to_string)
}

/// What `ssh -G` prints for this target: its configuration as `ssh` resolves it, without
/// connecting. `None` when `ssh -G` cannot run or fails.
pub(super) async fn ssh_dump(bare: &SshTarget) -> Option<String> {
    let mut argv = bare.ssh_argv(":");
    argv.insert(1, "-G".into());
    match Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        _ => None,
    }
}
