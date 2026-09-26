// SPDX-License-Identifier: GPL-3.0-or-later
//! `setup`, answered in the agent for the `min` subset on Debian and Ubuntu: the 17 collectors
//! ansible-core 2.19.12 runs for it, each reading what the reference reads, in the reference's
//! words.
//!
//! Anything the native cannot reproduce hands the task back to the Python module: an argument
//! outside `min` with the default fact path, another distribution, SELinux enabled, a host name
//! only DNS knows, a locale the module would replace. A fact the native cannot produce is never
//! guessed. Handing back is always safe here, because collecting changes nothing on the host.
//!
//! Every file is read under a [`Root`], the real `/` or a test directory, and every command is
//! found through the module's own `PATH` under the same root.

mod apparmor;
mod caps;
mod cmdline;
mod date_time;
mod distribution;
mod dns;
mod env;
mod fips;
mod local;
mod lsb;
mod pkg_mgr;
mod platform;
mod python;
mod selinux;
mod service_mgr;
mod ssh_pub_keys;
mod user;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use crate::modules::{Context, Run};
use crate::natives::common::{ArgSpec, invocation};
use crate::natives::{Native, NativeRun};

pub const NATIVE: Native = Native {
    name: "setup",
    aliases: &[],
    enabled: true,
    run: |args, context, cancelled| {
        let clock = Clock {
            deadline: context.timeout.map(|timeout| Instant::now() + timeout),
            cancelled,
        };
        match answer(args, &Root::real(), context, clock) {
            Ok(result) => NativeRun::Done(TaskResult(result)),
            Err(Stop::HandBack(reason)) => NativeRun::Fallback(reason),
            // What the Python path answers when the module outlives the task's `timeout`.
            Err(Stop::TimedOut) => NativeRun::Done(TaskResult::timed_out(
                context.timeout.unwrap_or_default().as_secs(),
            )),
            Err(Stop::Cancelled) => NativeRun::Cancelled,
        }
    },
};

/// Why collecting ended without the facts.
#[derive(Debug)]
pub enum Stop {
    /// Outside what the native reproduces: the payload runs instead.
    HandBack(String),
    /// A command the native ran outlived the task's `timeout`.
    TimedOut,
    /// The controller cancelled the task while a command was running.
    Cancelled,
}

impl From<String> for Stop {
    fn from(reason: String) -> Stop {
        Stop::HandBack(reason)
    }
}

impl From<&str> for Stop {
    fn from(reason: &str) -> Stop {
        Stop::HandBack(reason.to_string())
    }
}

/// What bounds every command the native runs: the task's deadline and the controller's cancel,
/// honoured by the executor `command` runs under.
#[derive(Clone, Copy)]
pub struct Clock<'a> {
    pub deadline: Option<Instant>,
    pub cancelled: &'a dyn Fn() -> bool,
}

/// A task with no `timeout` that nothing cancels.
pub fn unbounded() -> Clock<'static> {
    fn never() -> bool {
        false
    }
    Clock {
        deadline: None,
        cancelled: &never,
    }
}

const DEFAULT_FACT_PATH: &str = "/etc/ansible/facts.d";

/// `setup`'s `argument_spec` in ansible-core 2.19.12.
const SPEC: &[ArgSpec] = &[
    ArgSpec {
        name: "gather_subset",
        aliases: &[],
        default: || Value::from(vec!["all"]),
    },
    ArgSpec {
        name: "gather_timeout",
        aliases: &[],
        default: || Value::from(10),
    },
    ArgSpec {
        name: "filter",
        aliases: &[],
        default: || Value::Array(Vec::new()),
    },
    ArgSpec {
        name: "fact_path",
        aliases: &[],
        default: || Value::from(DEFAULT_FACT_PATH),
    },
];

/// The collectors `setup` runs for `min`, its `minimal_gather_subset`.
const MIN: &[&str] = &[
    "apparmor",
    "caps",
    "cmdline",
    "date_time",
    "distribution",
    "dns",
    "env",
    "fips",
    "local",
    "lsb",
    "pkg_mgr",
    "platform",
    "python",
    "selinux",
    "service_mgr",
    "ssh_pub_keys",
    "user",
];

/// What the task asks of `setup`, once the native knows it can answer it.
struct Request {
    /// `gather_subset` as the reference converts it, which is also what its `gather_subset` fact
    /// says.
    gather_subset: Vec<String>,
    /// `None` when the module would find no directory to read.
    fact_path: Option<String>,
}

/// The module's result, or the reason to hand the task back.
fn answer(
    args: &Map<String, Value>,
    root: &Root,
    context: &Context,
    clock: Clock,
) -> Result<Map<String, Value>, Stop> {
    let request = request(args)?;
    let facts = collect(&request, root, context, clock)?;
    let mut invocation = invocation(SPEC, args);
    invocation["module_args"]["gather_subset"] = Value::from(request.gather_subset);
    let mut result = Map::new();
    result.insert("ansible_facts".into(), Value::Object(facts));
    result.insert("invocation".into(), invocation);
    Ok(result)
}

/// The arguments, checked for what the native reproduces exactly: `min`, no filter, and a fact
/// path the module would find empty.
fn request(args: &Map<String, Value>) -> Result<Request, String> {
    if let Some(key) = args
        .keys()
        .find(|key| !SPEC.iter().any(|arg| arg.name == key.as_str()))
    {
        return Err(format!("argument '{key}' is outside the native setup"));
    }
    match args.get("filter") {
        None | Some(Value::Null) => {}
        Some(Value::Array(filter)) if filter.is_empty() => {}
        Some(_) => return Err("a filter is outside the native setup".into()),
    }
    match args.get("gather_timeout") {
        None | Some(Value::Null) => {}
        Some(timeout) if timeout.is_i64() => {}
        Some(_) => return Err("gather_timeout is not an integer".into()),
    }
    let gather_subset = gather_subset(args.get("gather_subset"))?;
    let fact_path = match args.get("fact_path") {
        None => Some(DEFAULT_FACT_PATH.to_string()),
        Some(Value::Null) => None,
        // `type='path'` expands `~` and variables, and the local collector globs under it.
        Some(Value::String(path))
            if path.starts_with('/') && !path.contains(['~', '$', '*', '?', '[']) =>
        {
            Some(path.clone())
        }
        Some(_) => return Err("fact_path is not a plain absolute path".into()),
    };
    Ok(Request {
        gather_subset,
        fact_path,
    })
}

/// `gather_subset` converted as `type='list', elements='str'` converts it, if it selects exactly
/// the `min` collectors.
///
/// The reference starts from `min`, adds what is named, removes what is negated unless it was
/// also named, then adds back what the remaining collectors require: `service_mgr` needs
/// `platform` and `distribution`, `pkg_mgr` needs `distribution`. Negating a name outside `min`
/// removes nothing from it (measured: no other collector's fact ids reach into `min`). A
/// positive name outside `min` is left to the reference, which collects it or refuses it.
fn gather_subset(value: Option<&Value>) -> Result<Vec<String>, String> {
    let given: Vec<String> = match value {
        Some(Value::String(text)) => text.split(',').map(str::to_string).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or("gather_subset holds something other than strings")?,
        None | Some(Value::Null) => return Err("gather_subset defaults to all".into()),
        Some(_) => return Err("gather_subset is not a list".into()),
    };
    // `gather_subset or ['all']`: an empty list is everything, and the fact then says `['all']`.
    if given.is_empty() {
        return Err("an empty gather_subset is all".into());
    }
    let mut excluded: Vec<&str> = Vec::new();
    let mut named: Vec<&str> = Vec::new();
    for subset in &given {
        match subset.strip_prefix('!') {
            Some("min") => excluded.extend(MIN),
            Some(other) => excluded.push(other),
            None if subset == "min" => {}
            None if MIN.contains(&subset.as_str()) => named.push(subset),
            None => return Err(format!("gather_subset '{subset}' is outside min")),
        }
    }
    let mut kept: Vec<&str> = MIN
        .iter()
        .copied()
        .filter(|name| !excluded.contains(name) || named.contains(name))
        .collect();
    let requires: &[(&str, &[&str])] = &[
        ("service_mgr", &["platform", "distribution"]),
        ("pkg_mgr", &["distribution"]),
    ];
    for (collector, required) in requires {
        if kept.contains(collector) {
            kept.extend(
                required
                    .iter()
                    .filter(|name| !kept.contains(name))
                    .collect::<Vec<_>>(),
            );
        }
    }
    if kept.len() != MIN.len() {
        return Err("gather_subset leaves out part of min".into());
    }
    Ok(given)
}

/// The facts of `min`, under the names the module returns them, or the reason to hand back.
///
/// The distribution's `ID` first: it decides whether the host is inside the subset at all for
/// one file read, where the interpreter and `lsb_release` cost tens of milliseconds.
fn collect(
    request: &Request,
    root: &Root,
    context: &Context,
    clock: Clock,
) -> Result<Map<String, Value>, Stop> {
    for key in context.environment.keys() {
        if key == "LANG" || key == "LANGUAGE" || key == "TZ" || key.starts_with("LC_") {
            return Err(format!(
                "the task sets {key}, which changes what the module reads of its locale"
            )
            .into());
        }
    }
    distribution::gate(root)?;
    let interpreter = context
        .interpreter
        .as_deref()
        .ok_or("no interpreter to read the python facts from")?;
    // `lsb_release` needs the module's environment, not the interpreter: it runs beside the
    // probe in the environment the module has when its interpreter adds nothing to the agent's,
    // and again after the probe when it did (Python sets `LC_CTYPE` in a C locale).
    let guess: Option<BTreeMap<String, String>> = std::env::vars_os()
        .map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect::<Option<BTreeMap<_, _>>>()
        .map(|mut env| {
            env.extend(context.environment.clone());
            env
        });
    // The early run cannot ask the task's cancel, which answers once and on this thread: it
    // stops at the deadline, or as soon as the probe ends without an answer.
    let halt = AtomicBool::new(false);
    let (probe, early) = std::thread::scope(|scope| {
        let early = guess.as_ref().map(|env| {
            scope.spawn(|| {
                let halted = || halt.load(Ordering::Relaxed);
                let early_clock = Clock {
                    deadline: clock.deadline,
                    cancelled: &halted,
                };
                LsbRelease::run(root, env, early_clock)
            })
        });
        let probe = python::probe(interpreter, clock);
        if probe.is_err() {
            halt.store(true, Ordering::Relaxed);
        }
        (probe, early.and_then(|thread| thread.join().ok()))
    });
    let probe = probe?;
    let mut env = probe.env.clone();
    env.extend(context.environment.clone());
    let lsb_release = match early {
        Some(early) if guess.as_ref() == Some(&env) => early?,
        _ => LsbRelease::run(root, &env, clock)?,
    };
    let host = Host {
        root,
        env,
        probe: &probe,
        clock,
    };
    let collected = [
        distribution::collect(&host, &lsb_release)?,
        apparmor::collect(&host),
        caps::collect(&host)?,
        cmdline::collect(&host),
        date_time::collect(&host)?,
        dns::collect(&host),
        env::collect(&host),
        fips::collect(&host),
        local::collect(&host, request.fact_path.as_deref())?,
        lsb::collect(&host, &lsb_release)?,
        pkg_mgr::collect(&host)?,
        platform::collect(&host)?,
        python::collect(&host),
        selinux::collect(&host)?,
        service_mgr::collect(&host)?,
        ssh_pub_keys::collect(&host)?,
        user::collect(&host)?,
    ];
    let mut facts = Map::new();
    for (name, value) in collected.into_iter().flatten() {
        facts.insert(format!("ansible_{}", name.replace('-', "_")), value);
    }
    facts.insert(
        "gather_subset".into(),
        Value::from(request.gather_subset.clone()),
    );
    facts.insert("module_setup".into(), Value::Bool(true));
    Ok(facts)
}

/// Where the collectors read: `/` on a host, a directory in a test.
pub struct Root(String);

impl Root {
    pub fn real() -> Root {
        Root(String::new())
    }

    #[cfg(test)]
    pub fn at(dir: &Path) -> Root {
        Root(dir.to_string_lossy().into_owned())
    }

    /// `path` under the root, spelled as given: a trailing `/` stays, and with it the reference's
    /// "is a directory" meaning of `os.path.exists('/etc/init/')`.
    pub fn path(&self, path: &str) -> PathBuf {
        PathBuf::from(format!("{}{path}", self.0))
    }

    pub fn exists(&self, path: &str) -> bool {
        self.path(path).exists()
    }

    pub fn is_file(&self, path: &str) -> bool {
        self.path(path).is_file()
    }

    /// `get_file_content(path)`: the text, stripped, or `None` when the file is missing,
    /// unreadable, not UTF-8 or blank.
    pub fn content(&self, path: &str) -> Option<String> {
        let bytes = std::fs::read(self.path(path)).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        // Python reads text with universal newlines.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let stripped = py_strip(&text);
        (!stripped.is_empty()).then(|| stripped.to_string())
    }

    /// `get_file_lines(path)`.
    pub fn lines(&self, path: &str) -> Vec<String> {
        self.content(path)
            .map(|text| splitlines(&text).into_iter().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

/// The host as the module would see it.
pub struct Host<'a> {
    pub root: &'a Root,
    /// The module's environment: the one its interpreter starts with, plus the task's.
    pub env: BTreeMap<String, String>,
    pub probe: &'a python::Probe,
    pub clock: Clock<'a>,
}

impl Host<'_> {
    pub fn bin_path(&self, name: &str) -> Option<PathBuf> {
        bin_path(self.root, &self.env, name)
    }

    pub fn run(&self, program: &Path, args: &[&str]) -> Result<Option<(i32, String)>, Stop> {
        run(&self.env, self.clock, program, args)
    }
}

/// `module.get_bin_path(name)`: the first executable file of that name along `PATH`, with
/// `/sbin`, `/usr/sbin` and `/usr/local/sbin` added when missing.
fn bin_path(root: &Root, env: &BTreeMap<String, String>, name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<String> = env
        .get("PATH")
        .map(String::as_str)
        .unwrap_or_default()
        .split(':')
        .map(str::to_string)
        .collect();
    for sbin in ["/sbin", "/usr/sbin", "/usr/local/sbin"] {
        if !dirs.iter().any(|dir| dir == sbin) && root.exists(sbin) {
            dirs.push(sbin.to_string());
        }
    }
    dirs.iter()
        .filter(|dir| !dir.is_empty())
        .map(|dir| root.path(&join(dir, name)))
        .find(|path| is_executable_file(path))
}

/// Where `subprocess` finds `name` when it is given without a directory: `PATH`, or
/// `/bin:/usr/bin` without one, and no extra directory.
fn exec_path(root: &Root, env: &BTreeMap<String, String>, name: &str) -> Option<PathBuf> {
    env.get("PATH")
        .map_or("/bin:/usr/bin", String::as_str)
        .split(':')
        .map(|dir| root.path(&join(dir, name)))
        .find(|path| is_executable_file(path))
}

/// `module.run_command([program, args...])`, through the executor `command` runs under, so the
/// task's `timeout` and cancel reach it: the exit code and standard output, or `None` when the
/// program cannot be started, which the module reports rather than answering.
///
/// `env` is set over the agent's own environment, of which the module's is a superset.
pub fn run(
    env: &BTreeMap<String, String>,
    clock: Clock,
    program: &Path,
    args: &[&str],
) -> Result<Option<(i32, String)>, Stop> {
    let timeout = match clock.deadline {
        Some(deadline) => Some(
            deadline
                .checked_duration_since(Instant::now())
                .ok_or(Stop::TimedOut)?,
        ),
        None => None,
    };
    let mut argv = vec![Value::from(program.to_string_lossy().into_owned())];
    argv.extend(args.iter().map(|arg| Value::from(*arg)));
    let mut command = Map::new();
    command.insert("argv".into(), Value::Array(argv));
    command.insert("strip_empty_ends".into(), Value::Bool(false));
    let context = Context {
        timeout,
        environment: env.clone(),
        ..Context::default()
    };
    match crate::modules::command::execute(&command, false, &context, clock.cancelled) {
        Run::Cancelled => Err(Stop::Cancelled),
        Run::Done(result) if result.0.contains_key("timedout") => Err(Stop::TimedOut),
        // Only a command that started has a `start`.
        Run::Done(result) if result.0.contains_key("start") => Ok(Some((
            result.0["rc"]
                .as_i64()
                .and_then(|rc| i32::try_from(rc).ok())
                .unwrap_or(-1),
            result.0["stdout"].as_str().unwrap_or_default().to_string(),
        ))),
        Run::Done(_) => Ok(None),
    }
}

/// `lsb_release -a`, run once for the two readers the reference runs it for: the `distro`
/// library, which finds it along `PATH` only, and the `lsb` collector, which uses
/// `get_bin_path`.
pub struct LsbRelease {
    /// For `distro`: the output when the command exited 0.
    pub distro: Option<String>,
    /// For the `lsb` collector: exit code and output, when the command was found.
    pub collector: Option<(i32, String)>,
}

impl LsbRelease {
    fn run(root: &Root, env: &BTreeMap<String, String>, clock: Clock) -> Result<LsbRelease, Stop> {
        let collector_path = bin_path(root, env, "lsb_release");
        let distro_path = exec_path(root, env, "lsb_release");
        let started = "lsb_release was found and could not be started";
        let collector = match &collector_path {
            Some(path) => Some(run(env, clock, path, &["-a"])?.ok_or(started)?),
            None => None,
        };
        let distro_run = if distro_path == collector_path {
            collector.clone()
        } else {
            match &distro_path {
                Some(path) => Some(run(env, clock, path, &["-a"])?.ok_or(started)?),
                None => None,
            }
        };
        Ok(LsbRelease {
            distro: distro_run.and_then(|(code, out)| (code == 0).then_some(out)),
            collector,
        })
    }
}

/// `os.path.join(dir, name)` for a plain name.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// `os.path.exists(path) and not os.path.isdir(path) and is_executable(path)`.
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| !meta.is_dir() && meta.permissions().mode() & 0o111 != 0)
}

/// Python's `str.isspace` for one character: Rust's whitespace plus the four separators
/// `\x1c`-`\x1f`.
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// `str.strip()`.
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(py_space)
}

/// `str.split()`.
pub fn py_split(text: &str) -> impl Iterator<Item = &str> {
    text.split(py_space).filter(|word| !word.is_empty())
}

/// `str.splitlines()`.
pub fn splitlines(text: &str) -> Vec<&str> {
    let breaks = |c| {
        matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        )
    };
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(breaks) {
        lines.push(&rest[..at]);
        let width = if rest[at..].starts_with("\r\n") {
            2
        } else {
            rest[at..].chars().next().map_or(1, char::len_utf8)
        };
        rest = &rest[at + width..];
    }
    if !rest.is_empty() {
        lines.push(rest);
    }
    lines
}

/// Runs the native `setup` for `volant-agent --native-facts <gather_subset> [<interpreter>]`,
/// printing the module's result, or the reason it hands back on stderr with status 2.
pub fn print(gather_subset: &str, interpreter: &str) -> i32 {
    let mut args = Map::new();
    args.insert("gather_subset".into(), Value::from(gather_subset));
    let context = Context {
        interpreter: Some(interpreter.to_string()),
        ..Context::default()
    };
    match answer(&args, &Root::real(), &context, unbounded()) {
        Ok(result) => {
            let result = crate::modules::module_result(result);
            println!("{}", Value::Object(result.0));
            0
        }
        Err(Stop::HandBack(reason)) => {
            eprintln!("volant-agent: the native setup hands back: {reason}");
            2
        }
        Err(stop) => {
            eprintln!("volant-agent: the native setup stopped: {stop:?}");
            2
        }
    }
}

#[cfg(test)]
pub mod tests {
    use serde_json::json;
    use volant_protocol::facts::NATIVE_FACT_KEYS;

    use super::*;

    /// A directory standing in for `/`, removed when dropped.
    pub struct FakeRoot(pub PathBuf);

    impl FakeRoot {
        pub fn new(name: &str) -> FakeRoot {
            let dir = std::env::temp_dir().join(format!(
                "volant-setup-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            FakeRoot(dir)
        }

        pub fn root(&self) -> Root {
            Root::at(&self.0)
        }

        pub fn write(&self, path: &str, content: &str) -> &Self {
            let full = self.0.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
            self
        }

        /// An executable shell script at `path` printing `output` and exiting with `code`.
        pub fn command(&self, path: &str, output: &str, code: i32) -> &Self {
            self.write(
                path,
                &format!("#!/bin/sh\ncat <<'EOF'\n{output}\nEOF\nexit {code}\n"),
            );
            let full = self.0.join(path.trim_start_matches('/'));
            std::fs::set_permissions(full, std::fs::Permissions::from_mode(0o755)).unwrap();
            self
        }

        /// An executable shell script at `path` running `body`.
        pub fn script(&self, path: &str, body: &str) -> &Self {
            self.write(path, &format!("#!/bin/sh\n{body}\n"));
            let full = self.0.join(path.trim_start_matches('/'));
            std::fs::set_permissions(full, std::fs::Permissions::from_mode(0o755)).unwrap();
            self
        }

        pub fn mkdir(&self, path: &str) -> &Self {
            std::fs::create_dir_all(self.0.join(path.trim_start_matches('/'))).unwrap();
            self
        }

        /// A host the probe does not have to run for: the interpreter's answer given, and `PATH`
        /// pointing into the fake root's own `usr/bin`.
        pub fn host<'a>(&self, root: &'a Root, probe: &'a python::Probe) -> Host<'a> {
            let mut env = probe.env.clone();
            env.insert("PATH".into(), "/usr/bin".into());
            Host {
                root,
                env,
                probe,
                clock: unbounded(),
            }
        }
    }

    impl Drop for FakeRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The interpreter's answer on an Ubuntu 24.04 host, as the probe reads it there.
    pub fn probe() -> python::Probe {
        python::Probe {
            python: json!({
                "version": {"major": 3, "minor": 12, "micro": 3, "releaselevel": "final", "serial": 0},
                "version_info": [3, 12, 3, "final", 0],
                "executable": "/usr/bin/python3",
                "has_sslcontext": true,
                "type": "cpython",
            }),
            python_version: "3.12.3".into(),
            bits: "64bit".into(),
            env: BTreeMap::from([
                ("HOME".to_string(), "/home/user".to_string()),
                ("LOGNAME".to_string(), "user".to_string()),
            ]),
            lc_time: Some("C.UTF-8".into()),
            tz_dst: "UTC".into(),
            selinux: Some(false),
            distro: None,
        }
    }

    pub const UBUNTU_OS_RELEASE: &str = r#"PRETTY_NAME="Ubuntu 24.04.4 LTS"
NAME="Ubuntu"
VERSION_ID="24.04"
VERSION="24.04.4 LTS (Noble Numbat)"
VERSION_CODENAME=noble
ID=ubuntu
ID_LIKE=debian
HOME_URL="https://www.ubuntu.com/"
SUPPORT_URL="https://help.ubuntu.com/"
BUG_REPORT_URL="https://bugs.launchpad.net/ubuntu/"
PRIVACY_POLICY_URL="https://www.ubuntu.com/legal/terms-and-policies/privacy-policy"
UBUNTU_CODENAME=noble
LOGO=ubuntu-logo
"#;

    pub const DEBIAN_OS_RELEASE: &str = r#"PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"
NAME="Debian GNU/Linux"
VERSION_ID="12"
VERSION="12 (bookworm)"
VERSION_CODENAME=bookworm
ID=debian
HOME_URL="https://www.debian.org/"
SUPPORT_URL="https://www.debian.org/support"
BUG_REPORT_URL="https://bugs.debian.org/"
"#;

    /// Every source a Debian 12 host can give the collectors, the DSA host key included, so that
    /// every key the native can produce is produced.
    pub fn debian_root(name: &str) -> FakeRoot {
        let fake = FakeRoot::new(name);
        fake.write("/etc/os-release", DEBIAN_OS_RELEASE)
            .write("/etc/debian_version", "12.7\n")
            .command(
                "/usr/bin/lsb_release",
                "Distributor ID:\tDebian\nDescription:\tDebian GNU/Linux 12 (bookworm)\nRelease:\t12\nCodename:\tbookworm",
                0,
            )
            .command("/usr/sbin/capsh", "Current: =ep\nBounding set =cap_chown", 0)
            .write("/etc/nsswitch.conf", "passwd: files systemd\nhosts: files dns\n")
            .write("/etc/passwd", "root:x:0:0:root:/root:/bin/bash\nuser:x:1000:1000:A User,,,:/home/user:/bin/bash\n")
            .write("/proc/1/comm", "systemd\n")
            .write("/proc/cmdline", "BOOT_IMAGE=/vmlinuz ro quiet\n")
            .write("/proc/sys/crypto/fips_enabled", "0\n")
            .write("/etc/resolv.conf", "nameserver 127.0.0.53\noptions edns0 trust-ad\nsearch example\n")
            .write("/etc/machine-id", "0123456789abcdef0123456789abcdef\n")
            .write("/etc/hosts", "127.0.0.1 localhost\n127.0.1.1 probe-hostname\n");
        for algo in ["dsa", "rsa", "ecdsa", "ed25519"] {
            fake.write(
                &format!("/etc/ssh/ssh_host_{algo}_key.pub"),
                &format!("ssh-{algo} AAAA{algo} root@probe-hostname\n"),
            );
        }
        fake
    }

    /// Every key the native can produce is in `NATIVE_FACT_KEYS`, and every key of the list is
    /// produced on a host that has all of their sources.
    ///
    /// What would make this red: a key added to a collector and not to the list, which the
    /// controller would then never trust the native for; or a key in the list the native never
    /// writes, which a play would find missing.
    #[test]
    fn on_a_host_with_every_source_the_native_writes_exactly_the_listed_keys() {
        let fake = debian_root("keys");
        let root = fake.root();
        let probe = probe();
        let host = fake.host(&root, &probe);
        let lsb = LsbRelease::run(host.root, &host.env, unbounded()).unwrap();
        let collected = [
            distribution::collect(&host, &lsb).unwrap(),
            apparmor::collect(&host),
            caps::collect(&host).unwrap(),
            cmdline::collect(&host),
            date_time::collect(&host).unwrap(),
            dns::collect(&host),
            env::collect(&host),
            fips::collect(&host),
            local::collect(&host, Some(DEFAULT_FACT_PATH)).unwrap(),
            lsb::collect(&host, &lsb).unwrap(),
            pkg_mgr::collect(&host).unwrap(),
            platform::collect_with(
                &host,
                &platform::Uname {
                    system: "Linux".into(),
                    node: "probe-hostname".into(),
                    release: "6.1.0-18-amd64".into(),
                    version: "#1 SMP PREEMPT_DYNAMIC Debian 6.1.76-1".into(),
                    machine: "x86_64".into(),
                },
            )
            .unwrap(),
            python::collect(&host),
            selinux::collect(&host).unwrap(),
            service_mgr::collect(&host).unwrap(),
            ssh_pub_keys::collect(&host).unwrap(),
            user::collect(&host).unwrap(),
        ];
        let mut keys: Vec<String> = collected
            .into_iter()
            .flatten()
            .map(|(name, _)| format!("ansible_{name}"))
            .chain(["gather_subset".to_string(), "module_setup".to_string()])
            .collect();
        keys.sort();
        assert_eq!(keys, NATIVE_FACT_KEYS);
    }

    /// The subsets that select exactly `min`, measured against ansible-core 2.19.12's own
    /// resolution, and the ones that do not.
    ///
    /// What would make this red: `!all` read as nothing at all, a negated `min` collector not
    /// taken off, a required collector not put back (`!platform` alone still runs `platform`,
    /// because `service_mgr` needs it), or a string not split on commas the way `type='list'`
    /// splits it.
    #[test]
    fn only_a_subset_that_resolves_to_min_is_answered() {
        for min in [
            json!(["min"]),
            json!(["!all"]),
            json!(["!hardware"]),
            json!(["!platform"]),
            json!(["!distribution"]),
            json!(["!all", "dns"]),
            json!(["!dns", "dns"]),
            json!(["!all", "!nothing"]),
            json!(["!machine_id"]),
            json!("!all,min"),
        ] {
            assert!(gather_subset(Some(&min)).is_ok(), "{min}");
        }
        assert_eq!(
            gather_subset(Some(&json!("!all,min"))).unwrap(),
            vec!["!all", "min"]
        );
        for not_min in [
            json!(["all"]),
            json!(["!all", "!min"]),
            json!(["!min", "min"]),
            json!(["!service_mgr", "!platform"]),
            json!(["!dns"]),
            json!(["!min", "dns"]),
            json!(["all", "!hardware"]),
            json!(["network"]),
            json!("min, !hardware"),
            json!([1]),
            json!(null),
            json!([]),
        ] {
            assert!(gather_subset(Some(&not_min)).is_err(), "{not_min}");
        }
        assert!(gather_subset(None).is_err(), "absent is all");
    }

    /// The arguments the native takes, and the ones it leaves to the module.
    ///
    /// What would make this red: a filter answered unfiltered, an argument the reference refuses
    /// answered, or a fact path the module would expand or glob read as given.
    #[test]
    fn arguments_outside_the_native_are_handed_back() {
        let args = |v: Value| v.as_object().unwrap().clone();
        let ok = request(&args(
            json!({"gather_subset": ["min"], "filter": [], "gather_timeout": 5}),
        ))
        .unwrap();
        assert_eq!(ok.fact_path.as_deref(), Some(DEFAULT_FACT_PATH));
        assert_eq!(
            request(&args(json!({"gather_subset": ["min"], "fact_path": null})))
                .unwrap()
                .fact_path,
            None
        );
        for refused in [
            json!({"gather_subset": ["min"], "filter": ["ansible_pkg_mgr"]}),
            json!({"gather_subset": ["min"], "filter": "*"}),
            json!({"gather_subset": ["min"], "gather_timeout": "10"}),
            json!({"gather_subset": ["min"], "fact_path": "~/facts"}),
            json!({"gather_subset": ["min"], "fact_path": "facts"}),
            json!({"gather_subset": ["min"], "fact_path": "/etc/$X"}),
            json!({"gather_subset": ["min"], "no_such": 1}),
            json!({"gather_subset": ["min"], "_ansible_check_mode": true}),
        ] {
            assert!(request(&args(refused.clone())).is_err(), "{refused}");
        }
    }

    /// The module's answer around the facts: `ansible_facts`, and an `invocation` whose
    /// `gather_subset` is the list the reference made of a string.
    ///
    /// `lsb_release` runs beside the probe in the agent's environment; the interpreter here
    /// starts with another one, so the answer must come from a second run in the module's.
    ///
    /// What would make this red: the string left as given in `module_args`, which the reference
    /// never prints; a default missing from `module_args`; or the early `lsb_release` kept when
    /// the module's environment differs from the one it ran in.
    #[test]
    fn the_answer_carries_the_facts_and_the_reference_s_invocation() {
        let fake = debian_root("answer");
        fake.write(
            "/etc/hosts",
            &format!("127.0.1.1 {}\n", platform::uname().node),
        )
        .write(
            "/usr/bin/lsb_release",
            "#!/bin/sh\necho \"Distributor ID:\t$LOGNAME\"\n",
        );
        std::fs::set_permissions(
            fake.0.join("usr/bin/lsb_release"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        // Safety: set before any thread of this test process reads the environment.
        unsafe { std::env::set_var("LOGNAME", "the-agent-s-own") };
        let root = fake.root();
        let interpreter = python::tests::fake_interpreter(&fake, &probe());
        let context = Context {
            interpreter: Some(interpreter),
            environment: BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            ..Context::default()
        };
        let args = json!({"gather_subset": "!all"})
            .as_object()
            .unwrap()
            .clone();
        let result = answer(&args, &root, &context, unbounded()).unwrap();
        assert_eq!(
            result["invocation"],
            json!({"module_args": {
                "gather_subset": ["!all"],
                "gather_timeout": 10,
                "filter": [],
                "fact_path": "/etc/ansible/facts.d",
            }})
        );
        assert_eq!(result["ansible_facts"]["gather_subset"], json!(["!all"]));
        assert_eq!(result["ansible_facts"]["module_setup"], json!(true));
        assert_eq!(
            result["ansible_facts"]["ansible_distribution"],
            json!("Debian")
        );
        assert_eq!(
            result["ansible_facts"]["ansible_lsb"]["id"],
            json!("user"),
            "the module's LOGNAME, not the agent's"
        );
    }

    /// A task environment that changes the locale or the time zone hands back: the module would
    /// apply it before its locale check and its clock, and the native reads neither.
    #[test]
    fn a_task_locale_or_time_zone_hands_back() {
        let fake = FakeRoot::new("task-env");
        for key in ["LANG", "LC_ALL", "LC_TIME", "LANGUAGE", "TZ"] {
            let context = Context {
                environment: BTreeMap::from([(key.to_string(), "C".to_string())]),
                ..Context::default()
            };
            let request = Request {
                gather_subset: vec!["min".into()],
                fact_path: None,
            };
            let Err(Stop::HandBack(reason)) =
                collect(&request, &fake.root(), &context, unbounded())
            else {
                panic!("{key} is answered");
            };
            assert!(reason.contains(key), "{reason}");
        }
    }

    /// Outside Debian and Ubuntu the native hands back before it starts anything: the interpreter
    /// here leaves a mark if it runs.
    ///
    /// What would make this red: the distribution checked after the probe, which makes every
    /// hand-back on another distribution cost an interpreter start and `lsb_release`.
    #[test]
    fn another_distribution_hands_back_before_the_interpreter_starts() {
        let fake = FakeRoot::new("gate-first");
        let mark = fake.0.join("probed");
        fake.write("/etc/os-release", "ID=fedora\nVERSION_ID=40\n")
            .script("/usr/bin/fake-python", &format!("touch {}", mark.display()));
        let context = Context {
            interpreter: Some(fake.0.join("usr/bin/fake-python").display().to_string()),
            environment: BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            ..Context::default()
        };
        let request = Request {
            gather_subset: vec!["min".into()],
            fact_path: None,
        };
        let Err(Stop::HandBack(reason)) = collect(&request, &fake.root(), &context, unbounded())
        else {
            panic!("fedora is answered");
        };
        assert!(reason.contains("fedora"), "{reason}");
        assert!(!mark.exists(), "the interpreter ran");
    }

    /// The interpreter and `lsb_release` both hang here. The task's `timeout` ends them as it
    /// ends a Python module, and a cancel ends them as it ends a command, both well before the
    /// thirty seconds they would take.
    ///
    /// What would make this red: a command run outside the executor, or the early `lsb_release`
    /// left running, which keeps the task waiting for it after the probe has stopped.
    #[test]
    fn a_hung_command_ends_at_the_timeout_or_the_cancel() {
        let fake = debian_root("hung");
        fake.script("/usr/bin/fake-python", "sleep 30")
            .script("/usr/bin/lsb_release", "sleep 30");
        let context = Context {
            interpreter: Some(fake.0.join("usr/bin/fake-python").display().to_string()),
            environment: BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            timeout: Some(std::time::Duration::from_secs(1)),
            ..Context::default()
        };
        let request = Request {
            gather_subset: vec!["min".into()],
            fact_path: None,
        };
        let started = Instant::now();
        let clock = Clock {
            deadline: Some(started + std::time::Duration::from_secs(1)),
            cancelled: &|| false,
        };
        let stop = collect(&request, &fake.root(), &context, clock).unwrap_err();
        assert!(matches!(stop, Stop::TimedOut), "{stop:?}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());

        let asked = std::cell::Cell::new(0);
        let cancelled = || {
            asked.set(asked.get() + 1);
            asked.get() > 5
        };
        let started = Instant::now();
        let clock = Clock {
            deadline: None,
            cancelled: &cancelled,
        };
        let stop = collect(&request, &fake.root(), &context, clock).unwrap_err();
        assert!(matches!(stop, Stop::Cancelled), "{stop:?}");
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
    }

    #[test]
    fn python_string_helpers_split_like_python() {
        assert_eq!(
            splitlines("a\r\nb\rc\n\nd\x1ce"),
            vec!["a", "b", "c", "", "d", "e"]
        );
        assert_eq!(splitlines("a\n"), vec!["a"]);
        assert_eq!(py_strip("\x1f a \u{3000}"), "a");
        assert_eq!(
            py_split(" a\x1cb  c ").collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }
}
