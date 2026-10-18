// SPDX-License-Identifier: GPL-3.0-or-later
//! `service_facts`, answered in the agent on a host run by systemd: the commands ansible-core
//! 2.19.12 runs, read the way it reads them.
//!
//! The reference asks the SysV `service` tool for `--status-all`, then systemd for
//! `list-units`, then `list-unit-files`, then `show <unit> --property=ActiveState` for each unit
//! file `list-units` did not name. The native runs the same commands, with the same locale, and
//! only reorders when they run: the SysV listing beside systemd's, and the `show`s beside each
//! other. None of them changes anything on the host.
//!
//! It hands back where the reference would warn, fail or take another road: no systemd, another
//! init tool (`chkconfig`, `initctl`, OpenRC, `rcctl`), a command that fails or prints what the
//! parser would trip on, and a line naming two of the states the reference picks among in a
//! random order.

use super::Native;
#[cfg(not(unix))]
use super::NativeRun;

#[cfg(unix)]
pub const NATIVE: Native = Native {
    name: "service_facts",
    aliases: &[],
    enabled: true,
    run: imp::run,
};

/// The agent is only ever uploaded to Linux hosts; elsewhere the task goes to the Python module.
#[cfg(not(unix))]
pub const NATIVE: Native = Native {
    name: "service_facts",
    aliases: &[],
    enabled: false,
    run: |_, _, _| NativeRun::Fallback("the native service_facts reads a Linux host".into()),
};

/// The reason guard's cases, for `natives::tests`.
#[cfg(all(test, target_os = "linux"))]
pub(super) use tests::secret_probes;

#[cfg(unix)]
mod systemd;
#[cfg(unix)]
mod sysv;
#[cfg(all(test, target_os = "linux"))]
mod tests;

#[cfg(unix)]
mod imp {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use serde_json::{Map, Value};
    use volant_protocol::TaskResult;

    use super::systemd::systemd;
    use super::sysv::sysv;
    use crate::modules::Context;
    use crate::modules::command::CANCEL_POLL;
    use crate::natives::NativeRun;
    use crate::natives::runtime::{Clock, Root, Stop, py_strip, splitlines};

    pub fn run(
        args: &Map<String, Value>,
        context: &Context,
        cancelled: &dyn Fn() -> bool,
    ) -> NativeRun {
        let clock = Clock {
            deadline: context.timeout.map(|timeout| Instant::now() + timeout),
            cancelled,
        };
        let mut env: BTreeMap<String, String> = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .collect();
        env.extend(context.environment.clone());
        match answer(args, &Root::real(), &env, clock) {
            Ok(result) => NativeRun::Done(TaskResult(result)),
            Err(Stop::HandBack(reason)) => NativeRun::Fallback(reason),
            // What the Python path answers when the module outlives the task's `timeout`.
            Err(Stop::TimedOut) => NativeRun::Done(TaskResult::timed_out(
                context.timeout.unwrap_or_default().as_secs(),
            )),
            Err(Stop::Cancelled) => NativeRun::Cancelled,
        }
    }

    /// Python's `str.isspace`, which `\s`, `split()` and `rstrip()` use.
    pub(super) fn py_space(c: char) -> bool {
        c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
    }

    pub(super) fn answer(
        args: &Map<String, Value>,
        root: &Root,
        env: &BTreeMap<String, String>,
        clock: Clock,
    ) -> Result<Map<String, Value>, Stop> {
        if let Some(key) = args.keys().next() {
            return Err(format!("the argument {key} is outside the native service_facts").into());
        }
        for other in ["chkconfig", "initctl", "rc-status", "rc-update", "rcctl"] {
            if bin_path(root, env, other, &[])?.is_some() {
                return Err(format!("{other} is found, and the module then reads it").into());
            }
        }
        // `is_systemd_managed`: systemctl along PATH, and one of sd_booted's canaries.
        let managed = bin_path(root, env, "systemctl", &[])?.is_some()
            && [
                "/run/systemd/system/",
                "/dev/.run/systemd/",
                "/dev/.systemd/",
            ]
            .iter()
            .any(|canary| root.exists(canary));
        if !managed {
            return Err("the host is not run by systemd".into());
        }
        let systemctl = bin_path(root, env, "systemctl", &["/usr/bin", "/usr/local/bin"])?
            .ok_or("systemctl is not found")?;
        let service = bin_path(root, env, "service", &[])?;
        for path in std::iter::once(&systemctl).chain(&service) {
            if !plain(&path.to_string_lossy(), "/") {
                return Err(format!("{} is not a plain path", path.display()).into());
            }
        }
        let mut env = env.clone();
        let locale = best_locale(root, &env, clock)?;
        env.insert("LANG".into(), locale.clone());
        env.insert("LC_ALL".into(), locale);

        // The SysV listing and systemd's do not read each other: they run side by side, on
        // threads that cannot ask the task's cancel, which answers once and on this thread.
        // They stop at the deadline, or when this thread raises `halt` on the cancel.
        let halt = AtomicBool::new(false);
        let halted = || halt.load(Ordering::Relaxed);
        let halted: &(dyn Fn() -> bool + Sync) = &halted;
        let deadline = clock.deadline;
        let worker = move || Clock {
            deadline,
            cancelled: halted,
        };
        let env = &env;
        let systemctl = systemctl.as_path();
        let (sysv, systemd) = std::thread::scope(|scope| {
            let (sent, answer) = mpsc::channel();
            let sysv_sent = sent.clone();
            scope.spawn(move || {
                let listed = match service {
                    Some(service) => sysv(root, &service, env, worker()),
                    None => Ok(Vec::new()),
                };
                let _ = sysv_sent.send(Part::Sysv(listed));
            });
            scope.spawn(move || {
                let _ = sent.send(Part::Systemd(systemd(systemctl, env, worker)));
            });
            let (mut sysv, mut systemd) = (None, None);
            while sysv.is_none() || systemd.is_none() {
                match answer.recv_timeout(CANCEL_POLL) {
                    Ok(Part::Sysv(listed)) => sysv = Some(listed),
                    Ok(Part::Systemd(listed)) => systemd = Some(listed),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(Stop::from("a listing thread ended without an answer"));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if (clock.cancelled)() {
                            halt.store(true, Ordering::Relaxed);
                            return Err(Stop::Cancelled);
                        }
                    }
                }
            }
            Ok((sysv.expect("received"), systemd.expect("received")))
        })?;
        // A deadline or a cancel on either side wins over the other side's hand-back: handing
        // back would run the module again, past a deadline already spent.
        if let Err(stop @ (Stop::TimedOut | Stop::Cancelled)) = systemd {
            return Err(stop);
        }
        // `all_services.update(...)`, SysV first.
        let mut services = Map::new();
        for (name, service) in sysv?.into_iter().chain(systemd?) {
            services.insert(name, Value::Object(service));
        }
        if services.is_empty() {
            return Err("no service was found, which the module reports as skipped".into());
        }
        let mut facts = Map::new();
        facts.insert("services".into(), Value::Object(services));
        let mut invocation = Map::new();
        invocation.insert("module_args".into(), Value::Object(Map::new()));
        let mut result = Map::new();
        result.insert("ansible_facts".into(), Value::Object(facts));
        result.insert("invocation".into(), Value::Object(invocation));
        Ok(result)
    }

    /// Services by name, in the order the reference's dict gets them.
    pub(super) type Services = Vec<(String, Map<String, Value>)>;

    pub(super) type Listed = Result<Services, Stop>;

    enum Part {
        Sysv(Listed),
        Systemd(Listed),
    }

    /// Only characters a shell leaves as they are: such a word means the same run directly as
    /// through `/bin/sh -c`, which is how the reference runs its systemctl commands.
    pub(super) fn plain(word: &str, extra: &str) -> bool {
        !word.is_empty()
            && word
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-:@".contains(c) || extra.contains(c))
    }

    /// `get_bin_path(name, opt_dirs)`: the option directories that exist, then `PATH`, then the
    /// `sbin` directories `PATH` lacks, and the first executable file of that name. A relative
    /// `PATH` entry depends on the module's working directory: the native hands back.
    pub(super) fn bin_path(
        root: &Root,
        env: &BTreeMap<String, String>,
        name: &str,
        opt_dirs: &[&str],
    ) -> Result<Option<PathBuf>, String> {
        let mut dirs: Vec<&str> = opt_dirs
            .iter()
            .copied()
            .filter(|dir| root.exists(dir))
            .collect();
        dirs.extend(env.get("PATH").map_or("", String::as_str).split(':'));
        for sbin in ["/sbin", "/usr/sbin", "/usr/local/sbin"] {
            if !dirs.contains(&sbin) && root.exists(sbin) {
                dirs.push(sbin);
            }
        }
        for dir in dirs.into_iter().filter(|dir| !dir.is_empty()) {
            if !dir.starts_with('/') {
                return Err("PATH holds a relative directory".into());
            }
            let path = root.path(&format!("{}/{name}", dir.trim_end_matches('/')));
            let executable = std::fs::metadata(&path)
                .is_ok_and(|meta| !meta.is_dir() && meta.permissions().mode() & 0o111 != 0);
            if executable {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    /// `module.run_command`: exit code and standard output. A command that cannot start makes
    /// the module fail, and output that is not UTF-8 would reach the result escaped: both hand
    /// back.
    pub(super) fn command(
        env: &BTreeMap<String, String>,
        clock: Clock,
        program: &Path,
        args: &[&str],
    ) -> Result<(i32, String), Stop> {
        let (rc, stdout) = crate::natives::runtime::run(env, clock, program, args)?
            .ok_or_else(|| format!("{} could not be started", program.display()))?;
        if stdout.contains('\u{fffd}') {
            return Err(
                format!("{} printed something that is not UTF-8", program.display()).into(),
            );
        }
        Ok((rc, stdout))
    }

    /// `get_best_parsable_locale`: the first of the reference's preferences `locale -a` lists,
    /// `C` when it lists none of them or cannot say.
    fn best_locale(
        root: &Root,
        env: &BTreeMap<String, String>,
        clock: Clock,
    ) -> Result<String, Stop> {
        const PREFERENCES: [&str; 6] = [
            "C.utf8",
            "C.UTF-8",
            "en_US.utf8",
            "en_US.UTF-8",
            "C",
            "POSIX",
        ];
        let Some(locale) = bin_path(root, env, "locale", &[])? else {
            return Ok("C".into());
        };
        let (rc, out) = command(env, clock, &locale, &["-a"])?;
        if rc != 0 || out.is_empty() {
            return Ok("C".into());
        }
        let available = splitlines(py_strip(&out));
        Ok(PREFERENCES
            .iter()
            .find(|preference| available.contains(preference))
            .unwrap_or(&"C")
            .to_string())
    }
}
