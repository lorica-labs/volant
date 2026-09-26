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

    use crate::modules::Context;
    use crate::modules::command::CANCEL_POLL;
    use crate::natives::NativeRun;
    use crate::natives::setup::{Clock, Root, Stop, py_split, py_strip, splitlines};

    /// The states `SystemctlScanService` reads anywhere on a line but the last word.
    const BAD_STATES: [&str; 3] = ["not-found", "masked", "failed"];

    /// How many `systemctl show` run at once.
    const SHOWS_AT_ONCE: usize = 8;

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
    fn py_space(c: char) -> bool {
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

    type Listed = Result<Vec<(String, Map<String, Value>)>, Stop>;

    enum Part {
        Sysv(Listed),
        Systemd(Listed),
    }

    /// Only characters a shell leaves as they are: such a word means the same run directly as
    /// through `/bin/sh -c`, which is how the reference runs its systemctl commands.
    fn plain(word: &str, extra: &str) -> bool {
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
                return Err(format!("PATH holds the relative directory {dir}"));
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
    fn command(
        env: &BTreeMap<String, String>,
        clock: Clock,
        program: &Path,
        args: &[&str],
    ) -> Result<(i32, String), Stop> {
        let (rc, stdout) = crate::natives::setup::run(env, clock, program, args)?
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

    /// `_list_sysvinit`: `service --status-all`, read with the reference's expression.
    fn sysv(root: &Root, service: &Path, env: &BTreeMap<String, String>, clock: Clock) -> Listed {
        let (rc, stdout) = command(env, clock, service, &["--status-all"])?;
        if rc == 4 && !root.exists("/etc/init.d") {
            return Ok(Vec::new());
        }
        if rc != 0 {
            return Err(
                format!("service --status-all exited {rc}, which the module warns about").into(),
            );
        }
        Ok(status_all(&stdout)
            .into_iter()
            .map(|(name, running)| {
                let mut service = Map::new();
                service.insert("name".into(), Value::from(name.as_str()));
                service.insert(
                    "state".into(),
                    Value::from(if running { "running" } else { "stopped" }),
                );
                service.insert("source".into(), Value::from("sysv"));
                (name, service)
            })
            .collect())
    }

    /// Every match of `^\s*\[ (?P<state>\+|\-) \]\s+(?P<name>.+)$` under `re.M`, found as
    /// `finditer` finds them: `\s` crosses line ends, and `\s+` gives characters back to `.+`
    /// when nothing else follows. `true` for `+`.
    pub(super) fn status_all(text: &str) -> Vec<(String, bool)> {
        let mut found = Vec::new();
        let mut at = 0;
        while at <= text.len() {
            if at > 0 && !text[..at].ends_with('\n') {
                match text[at..].find('\n') {
                    Some(end) => at += end + 1,
                    None => break,
                }
                continue;
            }
            match match_at(text, at) {
                Some((end, name, running)) => {
                    found.push((name, running));
                    at = end;
                }
                None => at += text[at..].chars().next().map_or(1, char::len_utf8),
            }
        }
        found
    }

    fn match_at(text: &str, start: usize) -> Option<(usize, String, bool)> {
        let rest = &text[start..];
        let skipped = rest.len() - rest.trim_start_matches(py_space).len();
        let after = &rest[skipped..];
        let running = if after.starts_with("[ + ]") {
            true
        } else if after.starts_with("[ - ]") {
            false
        } else {
            return None;
        };
        let q = start + skipped + "[ + ]".len();
        let spaces: Vec<(usize, char)> = text[q..]
            .char_indices()
            .take_while(|(_, c)| py_space(*c))
            .collect();
        let run_end = spaces.last().map_or(0, |(offset, c)| offset + c.len_utf8());
        // `\s+` takes at least one; `.+` then needs a character that is not a line end.
        for count in (1..=spaces.len()).rev() {
            let from = q + spaces.get(count).map_or(run_end, |(offset, _)| *offset);
            if text[from..].starts_with('\n') || from == text.len() {
                continue;
            }
            let end = text[from..].find('\n').map_or(text.len(), |end| from + end);
            return Some((end, text[from..end].to_string(), running));
        }
        None
    }

    /// `SystemctlScanService`: `list-units`, then `list-unit-files`, then a `show` for each unit
    /// file the first did not list. The `show`s run beside each other and are read in the
    /// reference's order.
    fn systemd<'a>(
        systemctl: &Path,
        env: &BTreeMap<String, String>,
        clock: impl Fn() -> Clock<'a> + Copy + Send,
    ) -> Listed {
        let (rc, units) = command(
            env,
            clock(),
            systemctl,
            &[
                "list-units",
                "--no-pager",
                "--type",
                "service",
                "--all",
                "--plain",
            ],
        )?;
        if rc != 0 {
            return Err(
                format!("systemctl list-units exited {rc}, which the module warns about").into(),
            );
        }
        let mut services = from_units(&units)?;
        let (rc, files) = command(
            env,
            clock(),
            systemctl,
            &[
                "list-unit-files",
                "--no-pager",
                "--type",
                "service",
                "--all",
            ],
        )?;
        if rc != 0 {
            return Err(format!(
                "systemctl list-unit-files exited {rc}, which the module warns about"
            )
            .into());
        }
        let files = unit_files(&files)?;
        let unlisted = unlisted(&services, &files);
        let states = show_all(systemctl, env, clock, &unlisted)?;
        merge_unit_files(&mut services, &files, &states);
        Ok(services)
    }

    /// `_list_from_units`: one entry per line that mentions `.service`.
    pub(super) fn from_units(stdout: &str) -> Result<Vec<(String, Map<String, Value>)>, String> {
        let mut services: Vec<(String, Map<String, Value>)> = Vec::new();
        for line in stdout.split('\n').filter(|line| line.contains(".service")) {
            let fields: Vec<&str> = py_split(line).collect();
            if fields.len() < 4 {
                return Err(format!("the unit line {line:?} has fewer than four fields"));
            }
            let head = &fields[..fields.len() - 1];
            let bad: Vec<&str> = BAD_STATES
                .iter()
                .copied()
                .filter(|bad| head.contains(bad))
                .collect();
            // The reference tries its states in a frozenset's order, which changes with each
            // run's hash seed: with two on one line, its answer is a coin toss.
            let status = match bad[..] {
                [] => fields[2],
                [one] => one,
                _ => return Err(format!("the unit line {line:?} names two failure states")),
            };
            let state = if fields[3] == "running" {
                "running"
            } else {
                "stopped"
            };
            let mut service = Map::new();
            service.insert("name".into(), Value::from(fields[0]));
            service.insert("state".into(), Value::from(state));
            service.insert("status".into(), Value::from(status));
            service.insert("source".into(), Value::from("systemd"));
            insert(&mut services, fields[0], service);
        }
        Ok(services)
    }

    /// A dict's assignment: an existing key keeps its place and takes the new value.
    fn insert(
        services: &mut Vec<(String, Map<String, Value>)>,
        name: &str,
        service: Map<String, Value>,
    ) {
        match services.iter_mut().find(|(known, _)| known == name) {
            Some(slot) => slot.1 = service,
            None => services.push((name.to_string(), service)),
        }
    }

    /// `list-unit-files`' lines that mention `.service`, as name and state. A line with fewer
    /// than two words makes the reference raise.
    pub(super) fn unit_files(stdout: &str) -> Result<Vec<(String, String)>, String> {
        stdout
            .split('\n')
            .filter(|line| line.contains(".service"))
            .map(|line| {
                let mut words = py_split(line);
                match (words.next(), words.next()) {
                    (Some(name), Some(state)) => Ok((name.to_string(), state.to_string())),
                    _ => Err(format!(
                        "the unit file line {line:?} has fewer than two words"
                    )),
                }
            })
            .collect()
    }

    /// The unit files the reference runs `show` for: those not yet in its dict when their line
    /// comes, each once.
    pub(super) fn unlisted(
        services: &[(String, Map<String, Value>)],
        files: &[(String, String)],
    ) -> Vec<String> {
        let mut unlisted: Vec<String> = Vec::new();
        for (name, _) in files {
            if !services.iter().any(|(known, _)| known == name) && !unlisted.contains(name) {
                unlisted.push(name.clone());
            }
        }
        unlisted
    }

    /// `_list_from_unit_files`' updates, given each unlisted unit's `show` answer.
    pub(super) fn merge_unit_files(
        services: &mut Vec<(String, Map<String, Value>)>,
        files: &[(String, String)],
        states: &BTreeMap<String, (i32, String)>,
    ) {
        for (name, status) in files {
            match services.iter_mut().find(|(known, _)| known == name) {
                None => {
                    let (rc, stdout) = &states[name];
                    let state = if *rc == 0 && !stdout.is_empty() {
                        stdout
                            .replace("ActiveState=", "")
                            .trim_end_matches(py_space)
                            .to_string()
                    } else {
                        "unknown".to_string()
                    };
                    let mut service = Map::new();
                    service.insert("name".into(), Value::from(name.as_str()));
                    service.insert("state".into(), Value::from(state));
                    service.insert("status".into(), Value::from(status.as_str()));
                    service.insert("source".into(), Value::from("systemd"));
                    services.push((name.clone(), service));
                }
                Some((_, service)) => {
                    let current = service["status"].as_str().unwrap_or_default();
                    if !BAD_STATES.contains(&current) {
                        service.insert("status".into(), Value::from(status.as_str()));
                    }
                }
            }
        }
    }

    /// `systemctl show <unit> --property=ActiveState` for every unit, at most `SHOWS_AT_ONCE`
    /// at a time. A name a shell would change goes through `/bin/sh -c`, as the reference sends
    /// every one of them.
    fn show_all<'a>(
        systemctl: &Path,
        env: &BTreeMap<String, String>,
        clock: impl Fn() -> Clock<'a> + Copy + Send,
        units: &[String],
    ) -> Result<BTreeMap<String, (i32, String)>, Stop> {
        let chunk = units.len().div_ceil(SHOWS_AT_ONCE).max(1);
        std::thread::scope(|scope| {
            let workers: Vec<_> = units
                .chunks(chunk)
                .map(|units| {
                    scope.spawn(move || {
                        units
                            .iter()
                            .map(|unit| Ok((unit.clone(), show(systemctl, env, clock(), unit)?)))
                            .collect::<Result<Vec<_>, Stop>>()
                    })
                })
                .collect();
            let mut states = BTreeMap::new();
            for worker in workers {
                states.extend(worker.join().expect("a show thread panicked")?);
            }
            Ok(states)
        })
    }

    fn show(
        systemctl: &Path,
        env: &BTreeMap<String, String>,
        clock: Clock,
        unit: &str,
    ) -> Result<(i32, String), Stop> {
        if plain(unit, "") {
            return command(
                env,
                clock,
                systemctl,
                &["show", unit, "--property=ActiveState"],
            );
        }
        let line = format!("{} show {unit} --property=ActiveState", systemctl.display());
        command(env, clock, Path::new("/bin/sh"), &["-c", &line])
    }

    #[cfg(test)]
    mod tests {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        use serde_json::json;

        use super::*;
        use crate::natives::setup::unbounded;

        /// `list-units` on an Ubuntu 24.04 host with systemd 255, cut down and sanitised (disk
        /// ids zeroed), plus the lines whose description names a failure state: the reference
        /// reads every word but the last, so `grub-initrd-fallback` and
        /// `update-notifier-download` come out `failed`, as measured on that host, while
        /// `demo-tail` (added here) ends on one and does not.
        const UNITS: &str = r"UNIT                                                          LOAD      ACTIVE   SUB     DESCRIPTION
apparmor.service                                              loaded    active   exited  Load AppArmor profiles
apport-autoreport.service                                     loaded    inactive dead    Process error reports when automatic reporting is enabled
auditd.service                                                not-found inactive dead    auditd.service
cron.service                                                  loaded    active   running Regular background program processing daemon
fwupd-refresh.service                                         loaded    failed   failed  Refresh fwupd metadata and update motd
grub-initrd-fallback.service                                  loaded    inactive dead    GRUB failed boot detection
systemd-fsck@dev-disk-by\x2duuid-0000\x2d0000.service         loaded    active   exited  File System Check on /dev/disk/by-uuid/0000-0000
systemd-journald.service                                      loaded    active   running Journal Service
update-notifier-download.service                              loaded    inactive dead    Download data for packages that failed at package install time
demo-tail.service                                             loaded    inactive dead    Retry what failed
user@1000.service                                             loaded    active   running User Manager for UID 1000
zfs-mount.service                                             not-found inactive dead    zfs-mount.service

Legend: LOAD   → Reflects whether the unit definition was properly loaded.
        ACTIVE → The high-level unit activation state, i.e. generalization of SUB.
        SUB    → The low-level unit activation state, values depend on unit type.

12 loaded units listed.
To show all installed unit files use 'systemctl list-unit-files'.
";

        /// `list-unit-files` on the same host, cut down: templates, aliases, a masked unit, a
        /// `bad` one, `enabled-runtime`, and units `list-units` names as well.
        const FILES: &str = r"UNIT FILE                                    STATE           PRESET
apparmor.service                             enabled         enabled
apport-autoreport.service                    static          -
autovt@.service                              alias           -
cron.service                                 enabled         enabled
cryptdisks.service                           masked          enabled
dbus-org.freedesktop.resolve1.service        alias           -
dbus-org.freedesktop.timesync1.service       bad             enabled
fwupd-refresh.service                        static          -
getty@.service                               enabled         enabled
grub-initrd-fallback.service                 enabled         enabled
lxd-agent.service                            static          -
systemd-journald.service                     static          -
systemd-remount-fs.service                   enabled-runtime enabled
update-notifier-download.service             static          -
user@.service                                static          -

15 unit files listed.
";

        const SYSV: &str = " [ + ]  apparmor\n [ - ]  console-setup.sh\n [ + ]  cron\n [ - ]  cryptdisks\n [ + ]  procps\n [ - ]  uuidd\n";

        /// What `systemctl show <unit> --property=ActiveState` gave there: nothing and exit 1
        /// for a template, the target's state for an alias.
        const SHOWS: [(&str, &str, i32); 8] = [
            ("autovt@.service", "", 1),
            ("getty@.service", "", 1),
            ("user@.service", "", 1),
            ("cryptdisks.service", "ActiveState=inactive\n", 0),
            ("lxd-agent.service", "ActiveState=inactive\n", 0),
            ("systemd-remount-fs.service", "ActiveState=inactive\n", 0),
            (
                "dbus-org.freedesktop.resolve1.service",
                "ActiveState=active\n",
                0,
            ),
            (
                "dbus-org.freedesktop.timesync1.service",
                "ActiveState=inactive\n",
                0,
            ),
        ];

        /// ansible-core 2.19.12's own `ServiceScanService` and `SystemctlScanService`, run on
        /// the outputs above, merged as `main`
        /// merges them. No warning.
        const REFERENCE: &str = r#"{"apparmor": {"name": "apparmor", "source": "sysv", "state": "running"}, "apparmor.service": {"name": "apparmor.service", "source": "systemd", "state": "stopped", "status": "enabled"}, "apport-autoreport.service": {"name": "apport-autoreport.service", "source": "systemd", "state": "stopped", "status": "static"}, "auditd.service": {"name": "auditd.service", "source": "systemd", "state": "stopped", "status": "not-found"}, "autovt@.service": {"name": "autovt@.service", "source": "systemd", "state": "unknown", "status": "alias"}, "console-setup.sh": {"name": "console-setup.sh", "source": "sysv", "state": "stopped"}, "cron": {"name": "cron", "source": "sysv", "state": "running"}, "cron.service": {"name": "cron.service", "source": "systemd", "state": "running", "status": "enabled"}, "cryptdisks": {"name": "cryptdisks", "source": "sysv", "state": "stopped"}, "demo-tail.service": {"name": "demo-tail.service", "source": "systemd", "state": "stopped", "status": "inactive"}, "cryptdisks.service": {"name": "cryptdisks.service", "source": "systemd", "state": "inactive", "status": "masked"}, "dbus-org.freedesktop.resolve1.service": {"name": "dbus-org.freedesktop.resolve1.service", "source": "systemd", "state": "active", "status": "alias"}, "dbus-org.freedesktop.timesync1.service": {"name": "dbus-org.freedesktop.timesync1.service", "source": "systemd", "state": "inactive", "status": "bad"}, "fwupd-refresh.service": {"name": "fwupd-refresh.service", "source": "systemd", "state": "stopped", "status": "failed"}, "getty@.service": {"name": "getty@.service", "source": "systemd", "state": "unknown", "status": "enabled"}, "grub-initrd-fallback.service": {"name": "grub-initrd-fallback.service", "source": "systemd", "state": "stopped", "status": "failed"}, "lxd-agent.service": {"name": "lxd-agent.service", "source": "systemd", "state": "inactive", "status": "static"}, "procps": {"name": "procps", "source": "sysv", "state": "running"}, "systemd-fsck@dev-disk-by\\x2duuid-0000\\x2d0000.service": {"name": "systemd-fsck@dev-disk-by\\x2duuid-0000\\x2d0000.service", "source": "systemd", "state": "stopped", "status": "active"}, "systemd-journald.service": {"name": "systemd-journald.service", "source": "systemd", "state": "running", "status": "static"}, "systemd-remount-fs.service": {"name": "systemd-remount-fs.service", "source": "systemd", "state": "inactive", "status": "enabled-runtime"}, "update-notifier-download.service": {"name": "update-notifier-download.service", "source": "systemd", "state": "stopped", "status": "failed"}, "user@.service": {"name": "user@.service", "source": "systemd", "state": "unknown", "status": "static"}, "user@1000.service": {"name": "user@1000.service", "source": "systemd", "state": "running", "status": "active"}, "uuidd": {"name": "uuidd", "source": "sysv", "state": "stopped"}, "zfs-mount.service": {"name": "zfs-mount.service", "source": "systemd", "state": "stopped", "status": "not-found"}}"#;

        /// A host of fake commands under a directory of its own, removed at the end: `systemctl`
        /// and `service` print the outputs above, `locale -a` lists `C.utf8`.
        struct Host(PathBuf);

        impl Drop for Host {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        const SYSTEMCTL: &str = r#"#!/bin/sh
d="$(dirname "$0")/../../fixture"
case "$1" in
list-units) cat "$d/units.txt"; exit "$(cat "$d/units.rc" 2>/dev/null || echo 0)" ;;
list-unit-files) cat "$d/files.txt" ;;
show) [ -f "$d/show/$2" ] || { echo "ActiveState=$2"; exit 0; }; cat "$d/show/$2"; exit "$(cat "$d/show/$2.rc")" ;;
esac
"#;

        impl Host {
            fn new() -> Host {
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                let dir = PathBuf::from(format!(
                    "/tmp/volant-service-facts-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(dir.join("run/systemd/system")).unwrap();
                std::fs::create_dir_all(dir.join("fixture/show")).unwrap();
                let host = Host(dir);
                host.command("/usr/bin/systemctl", SYSTEMCTL);
                host.command(
                    "/usr/sbin/service",
                    "#!/bin/sh\ncat \"$(dirname \"$0\")/../../fixture/sysv.txt\"\n",
                );
                host.command(
                    "/usr/bin/locale",
                    "#!/bin/sh\nprintf 'C\\nC.utf8\\nPOSIX\\n'\n",
                );
                host.write("/fixture/units.txt", UNITS);
                host.write("/fixture/files.txt", FILES);
                host.write("/fixture/sysv.txt", SYSV);
                for (unit, stdout, rc) in SHOWS {
                    host.write(&format!("/fixture/show/{unit}"), stdout);
                    host.write(&format!("/fixture/show/{unit}.rc"), &rc.to_string());
                }
                host
            }

            fn write(&self, path: &str, content: &str) {
                let full = self.0.join(path.trim_start_matches('/'));
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(full, content).unwrap();
            }

            fn command(&self, path: &str, script: &str) {
                self.write(path, script);
                let full = self.0.join(path.trim_start_matches('/'));
                std::fs::set_permissions(full, std::fs::Permissions::from_mode(0o755)).unwrap();
            }

            fn answer_with(&self, clock: Clock) -> Result<Map<String, Value>, Stop> {
                let env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
                answer(&Map::new(), &Root::at(&self.0), &env, clock)
            }

            fn answer(&self) -> Result<Map<String, Value>, Stop> {
                self.answer_with(unbounded())
            }

            fn hands_back(&self) -> String {
                match self.answer() {
                    Err(Stop::HandBack(reason)) => reason,
                    other => panic!("answered where the native should hand back: {other:?}"),
                }
            }
        }

        /// The fixture host, answered as the reference's own scanners answer the same outputs.
        ///
        /// What would make this red: `list-unit-files` read before `list-units` (a unit in both
        /// would take its state from `show` and keep `list-units`' status); the last word of a
        /// description read for failure states, or the others not; a unit file's state written
        /// over a failure state; `show`'s output kept with its `ActiveState=` or its newline;
        /// a template's failed `show` read as anything but `unknown`; the SysV entries missing
        /// or given a `status`.
        #[test]
        fn a_host_is_answered_as_the_reference_parses_it() {
            let host = Host::new();
            let result = host.answer().unwrap();
            let reference: Value = serde_json::from_str(REFERENCE).unwrap();
            assert_eq!(result["ansible_facts"]["services"], reference);
            assert_eq!(result["invocation"], json!({"module_args": {}}));
        }

        /// `service` exiting 4 on a host without `/etc/init.d` (what RHEL 9 does) is no SysV
        /// listing at all, and no warning: systemd's units are answered alone.
        ///
        /// What would make this red: the exit read as a failure, which hands back, or its
        /// output read.
        #[test]
        fn a_service_tool_without_init_scripts_lists_nothing() {
            let host = Host::new();
            host.command(
                "/usr/sbin/service",
                "#!/bin/sh\necho ' [ + ]  ghost'\nexit 4\n",
            );
            let result = host.answer().unwrap();
            let services = result["ansible_facts"]["services"].as_object().unwrap();
            assert!(
                services
                    .values()
                    .all(|service| service["source"] == "systemd")
            );
            assert!(services.contains_key("cron.service"));
        }

        /// A unit line naming two failure states: the reference picks one in its set's order,
        /// which its hash seed changes from run to run. The native hands back rather than pick.
        #[test]
        fn two_failure_states_on_one_line_hand_back() {
            let host = Host::new();
            host.write(
                "/fixture/units.txt",
                "gone.service not-found failed failed gone.service\n",
            );
            assert!(host.hands_back().contains("names two failure states"));
        }

        /// Hosts where the reference warns, fails, or reads another tool hand back.
        ///
        /// What would make this red, one each: a unit line too short for the reference's
        /// indexing answered; a one-word unit file line answered (the reference raises); a
        /// failing `list-units` or `service --status-all` answered (the reference warns);
        /// `chkconfig` ignored; a host without systemd's canary answered; output that is not
        /// UTF-8 answered; an argument accepted (the module takes none).
        #[test]
        fn a_host_outside_the_subset_hands_back() {
            let host = Host::new();
            host.write("/fixture/units.txt", "short.service loaded active\n");
            assert!(host.hands_back().contains("fewer than four fields"));

            let host = Host::new();
            host.write("/fixture/files.txt", "lonely.service\n");
            assert!(host.hands_back().contains("fewer than two words"));

            let host = Host::new();
            host.write("/fixture/units.rc", "1");
            assert!(host.hands_back().contains("list-units exited 1"));

            let host = Host::new();
            host.command("/usr/sbin/chkconfig", "#!/bin/sh\n");
            assert!(host.hands_back().contains("chkconfig is found"));

            let host = Host::new();
            std::fs::remove_dir_all(host.0.join("run/systemd")).unwrap();
            assert_eq!(host.hands_back(), "the host is not run by systemd");

            let host = Host::new();
            std::fs::write(
                host.0.join("fixture/units.txt"),
                b"bad\xff.service a b c d\n",
            )
            .unwrap();
            assert!(host.hands_back().contains("is not UTF-8"));

            let host = Host::new();
            host.command("/usr/sbin/service", "#!/bin/sh\nexit 3\n");
            assert!(host.hands_back().contains("--status-all exited 3"));

            let host = Host::new();
            let args = json!({"x": 1});
            let reason = match answer(
                args.as_object().unwrap(),
                &Root::at(&host.0),
                &BTreeMap::new(),
                unbounded(),
            ) {
                Err(Stop::HandBack(reason)) => reason,
                other => panic!("an argument was accepted: {other:?}"),
            };
            assert!(reason.contains("the argument x"), "{reason}");
        }

        /// A hung command ends at the task's deadline with `TimedOut`, and a cancel with
        /// `Cancelled`, whichever thread runs it: `service` hangs here, beside systemd's
        /// listing.
        ///
        /// What would make this red: the SysV thread run without the deadline or the cancel
        /// (the test then waits the full sleep, and its limit fails it), or the cancel not
        /// asked while the threads run.
        #[test]
        fn a_hung_command_ends_at_the_deadline_or_the_cancel() {
            let host = Host::new();
            host.command("/usr/sbin/service", "#!/bin/sh\nexec sleep 60\n");
            let started = Instant::now();
            let never = || false;
            let clock = Clock {
                deadline: Some(Instant::now() + Duration::from_millis(500)),
                cancelled: &never,
            };
            assert!(matches!(host.answer_with(clock), Err(Stop::TimedOut)));
            let calls = AtomicUsize::new(0);
            let soon = || calls.fetch_add(1, Ordering::Relaxed) > 3;
            let clock = Clock {
                deadline: None,
                cancelled: &soon,
            };
            assert!(matches!(host.answer_with(clock), Err(Stop::Cancelled)));
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the hung command was waited for"
            );
        }

        /// A unit name a shell rewrites is sent through the shell, as the reference sends it:
        /// `\x2d` reaches systemctl as `x2d`.
        #[test]
        fn a_name_the_shell_rewrites_goes_through_the_shell() {
            let host = Host::new();
            let systemctl = host.0.join("usr/bin/systemctl");
            let env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
            let (rc, stdout) = show(&systemctl, &env, unbounded(), r"a\x2db.service").unwrap();
            assert_eq!((rc, stdout.as_str()), (0, "ActiveState=ax2db.service\n"));
            let (_, stdout) = show(&systemctl, &env, unbounded(), "a-b.service").unwrap();
            assert_eq!(stdout, "ActiveState=a-b.service\n");
        }

        /// `service --status-all` read with the reference's expression, on outputs whose
        /// answers were taken from Python's `re.finditer`.
        #[test]
        fn the_sysv_listing_is_read_as_python_reads_it() {
            let cases: [(&str, &[(&str, bool)]); 11] = [
                (
                    " [ + ]  apparmor\n [ - ]  cron\n",
                    &[("apparmor", true), ("cron", false)],
                ),
                (
                    " [ + ]  a\n\n  [ - ]\tb c \n [ ? ]  d\n",
                    &[("a", true), ("b c ", false)],
                ),
                (" [ + ]  \n", &[(" ", true)]),
                (" [ + ]  \n [ - ]  x\n", &[("[ - ]  x", true)]),
                ("[ + ] x", &[("x", true)]),
                (" [ + ]x\n", &[]),
                (" [ +]  y\n", &[]),
                ("junk [ + ]  z\n [ - ]  w", &[("w", false)]),
                (" [ + ]  \x1c v\n", &[("v", true)]),
                (" [ + ]  \n  \n", &[(" ", true)]),
                (" [ - ] \r\n", &[("\r", false)]),
            ];
            for (text, expected) in cases {
                let expected: Vec<(String, bool)> = expected
                    .iter()
                    .map(|(name, running)| ((*name).to_string(), *running))
                    .collect();
                assert_eq!(status_all(text), expected, "{text:?}");
            }
        }
    }
}
