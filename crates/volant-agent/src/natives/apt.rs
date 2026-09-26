// SPDX-License-Identifier: GPL-3.0-or-later
//! `apt`, answered in the agent when the task asks for what the host already has: a package
//! cache that is still fresh, packages that are all installed, or packages none of which is.
//! Every other task goes to the Python module, handed back before anything on the host changed.
//!
//! Read from ansible-core 2.19.12's `apt.py`, whose answers in those three cases are:
//!
//! - only a cache check (`update_cache`, or a `cache_valid_time`), the cache fresh:
//!   `{"changed": false, "cache_updated": false, "cache_update_time": T}`;
//! - `state: present`, every package `install ok installed`: the same three keys, after
//!   `apt-mark manual <packages>`, which the module runs whether or not anything changes;
//! - `state: absent`, no package installed: `{"changed": false}`.
//!
//! The native reads files only (`/var/lib/dpkg/status`, `/var/lib/apt/extended_states`, the
//! cache stamp) and takes no lock. dpkg replaces its status file by renaming a complete new one
//! over it, so a read sees one version or the other, never a mix; a journal left in
//! `/var/lib/dpkg/updates/` means the status file is not the whole story, and hands back.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use super::common::{ArgSpec, invocation};
use super::setup::Root;
use super::{Native, NativeRun};
use crate::modules::{Context, Run};

pub const NATIVE: Native = Native {
    name: "apt",
    aliases: &[],
    enabled: true,
    run,
};

const STATUS: &str = "/var/lib/dpkg/status";
const UPDATES: &str = "/var/lib/dpkg/updates";
const EXTENDED_STATES: &str = "/var/lib/apt/extended_states";
const STAMP: &str = "/var/lib/apt/periodic/update-success-stamp";
const LISTS: &str = "/var/lib/apt/lists";

/// What the module's `mark_installed_manually` looks for in `apt-mark`'s standard error before
/// retrying with `unmarkauto`.
const APT_MARK_INVALID_OP: &str = "Invalid operation";
const APT_MARK_INVALID_OP_DEB6: &str = "Usage: apt-mark [options] {markauto|unmarkauto} packages";

/// `apt`'s `argument_spec` in ansible-core 2.19.12, with its defaults and aliases.
const SPEC: &[ArgSpec] = &[
    ArgSpec {
        name: "state",
        aliases: &[],
        default: || "present".into(),
    },
    ArgSpec {
        name: "update_cache",
        aliases: &["update-cache"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "update_cache_retries",
        aliases: &[],
        default: || 5.into(),
    },
    ArgSpec {
        name: "update_cache_retry_max_delay",
        aliases: &[],
        default: || 12.into(),
    },
    ArgSpec {
        name: "cache_valid_time",
        aliases: &[],
        default: || 0.into(),
    },
    ArgSpec {
        name: "purge",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "package",
        aliases: &["pkg", "name"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "deb",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "default_release",
        aliases: &["default-release"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "install_recommends",
        aliases: &["install-recommends"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "force",
        aliases: &[],
        default: || false.into(),
    },
    // `"no"` in the spec; the module sets it to `None` before it answers, and the
    // `invocation` shows that.
    ArgSpec {
        name: "upgrade",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "dpkg_options",
        aliases: &[],
        default: || "force-confdef,force-confold".into(),
    },
    ArgSpec {
        name: "autoremove",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "autoclean",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "fail_on_autoremove",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "policy_rc_d",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "only_upgrade",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "force_apt_get",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "clean",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "allow_unauthenticated",
        aliases: &["allow-unauthenticated"],
        default: || false.into(),
    },
    ArgSpec {
        name: "allow_downgrade",
        aliases: &["allow-downgrade", "allow_downgrades", "allow-downgrades"],
        default: || false.into(),
    },
    ArgSpec {
        name: "allow_change_held_packages",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "lock_timeout",
        aliases: &[],
        default: || 60.into(),
    },
    ArgSpec {
        name: "auto_install_module_deps",
        aliases: &[],
        default: || true.into(),
    },
];

/// How the native takes each argument it accepts; an argument missing here hands back when
/// given at all.
#[derive(Clone, Copy)]
enum Take {
    /// `type='bool'`, any value: it only shapes a command the fast path never runs.
    Bool,
    /// `type='bool'` with no default: `null` stays `null`.
    OptBool,
    /// `type='bool'` that must read false: true asks for work the fast path does not do.
    False,
    /// `type='int'`, any value.
    Int,
    /// `type='int'` with no default.
    OptInt,
    /// `type='str'`, any value.
    Str,
}

const TAKEN: &[(&str, Take)] = &[
    ("update_cache", Take::OptBool),
    ("update_cache_retries", Take::Int),
    ("update_cache_retry_max_delay", Take::Int),
    ("purge", Take::False),
    ("install_recommends", Take::OptBool),
    ("force", Take::Bool),
    ("dpkg_options", Take::Str),
    ("autoremove", Take::False),
    ("autoclean", Take::False),
    ("fail_on_autoremove", Take::Bool),
    ("policy_rc_d", Take::OptInt),
    ("only_upgrade", Take::False),
    ("force_apt_get", Take::Bool),
    ("clean", Take::False),
    ("allow_unauthenticated", Take::Bool),
    ("allow_downgrade", Take::Bool),
    ("allow_change_held_packages", Take::Bool),
    ("lock_timeout", Take::Int),
    ("auto_install_module_deps", Take::Bool),
];

fn run(args: &Map<String, Value>, context: &Context, cancelled: &dyn Fn() -> bool) -> NativeRun {
    answer(args, &Root::real(), context, SystemTime::now(), cancelled)
}

fn answer(
    args: &Map<String, Value>,
    root: &Root,
    context: &Context,
    now: SystemTime,
    cancelled: &dyn Fn() -> bool,
) -> NativeRun {
    let deadline = context.timeout.map(|t| Instant::now() + t);
    match plan(args, root, &context.environment, now) {
        Err(reason) => NativeRun::Fallback(reason),
        Ok(plan) => finish(plan, context, deadline, cancelled),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Present,
    Absent,
}

/// The arguments, as `AnsibleModule` would have validated them, inside the fast path.
#[derive(Debug)]
struct Request {
    state: State,
    packages: Vec<String>,
    update_cache: bool,
    cache_valid_time: i64,
    module_args: Map<String, Value>,
}

/// The answer, and the `apt-mark` run that comes with it for `state: present`.
#[derive(Debug)]
struct Plan {
    result: Map<String, Value>,
    mark: Option<(PathBuf, Vec<String>)>,
}

/// Decides, reading only. `Err` is the reason the task goes to the Python module.
fn plan(
    args: &Map<String, Value>,
    root: &Root,
    task_env: &BTreeMap<String, String>,
    now: SystemTime,
) -> Result<Plan, String> {
    if task_env.contains_key("TZ") {
        return Err("the task sets TZ, which moves the module's local time".into());
    }
    let request = request(args)?;
    let status = read(root, STATUS)?.ok_or("the dpkg status file is missing")?;
    let status = stanzas(&status);
    if !is_installed(&status, "python3-apt", None) {
        return Err("python3-apt is not installed, and the module would install it".into());
    }
    if std::fs::read_dir(root.path(UPDATES)).is_ok_and(|mut dir| dir.next().is_some()) {
        return Err("dpkg has pending updates in its journal".into());
    }

    let (seconds, micros) = match cache_mtime(root)? {
        Some((sec, nsec)) => from_timestamp(sec, nsec),
        None => (0, 0),
    };
    let now = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the clock is before 1970")?;
    let now_seconds = i64::try_from(now.as_secs()).map_err(|_| "the clock is out of range")?;
    // The module compares naive local times and makes `cache_update_time` with `mktime`: both
    // equal their UTC counterparts only while the zone keeps one offset around the stamp and
    // from it to now.
    let offsets: Option<Vec<libc::c_long>> = [seconds - 3600, seconds, seconds + 3600, now_seconds]
        .into_iter()
        .map(utc_offset)
        .collect();
    if !offsets.is_some_and(|o| o.windows(2).all(|w| w[0] == w[1])) {
        return Err("the local time changes offset around the cache stamp".into());
    }
    if request.update_cache || request.cache_valid_time != 0 {
        let stamp = i128::from(seconds) * 1_000_000 + i128::from(micros);
        let valid = i128::from(request.cache_valid_time) * 1_000_000;
        // `datetime.now()` floors to the microsecond.
        let now_micros = i128::from(now_seconds) * 1_000_000 + i128::from(now.subsec_micros());
        if stamp + valid < now_micros {
            return Err(if request.cache_valid_time == 0 {
                "update_cache without cache_valid_time".into()
            } else {
                "the package cache is older than cache_valid_time".into()
            });
        }
    }

    let mut result = Map::new();
    result.insert(
        "invocation".into(),
        Value::Object(Map::from_iter([(
            "module_args".to_string(),
            Value::Object(request.module_args),
        )])),
    );
    let cache = |result: &mut Map<String, Value>| {
        result.insert("changed".into(), false.into());
        result.insert("cache_updated".into(), false.into());
        result.insert("cache_update_time".into(), seconds.into());
    };
    if request.packages.is_empty() {
        if !request.update_cache && request.cache_valid_time == 0 {
            return Err("nothing to check, install or remove".into());
        }
        cache(&mut result);
        return Ok(Plan { result, mark: None });
    }
    match request.state {
        State::Absent => {
            if let Some(name) = request.packages.iter().find(|name| {
                status.get(name.as_str()).is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|e| !matches!(e.current(), "not-installed" | "config-files"))
                })
            }) {
                return Err(format!("{name} is installed, or partly"));
            }
            result.insert("changed".into(), false.into());
            Ok(Plan { result, mark: None })
        }
        State::Present => {
            // `dpkg` is always installed for the native architecture.
            let native_arch = match status.get("dpkg").map(Vec::as_slice) {
                Some([dpkg]) => dpkg.architecture,
                _ => return Err("the native architecture is not dpkg's alone".into()),
            };
            if let Some(name) = request
                .packages
                .iter()
                .find(|name| !is_installed(&status, name, Some(native_arch)))
            {
                return Err(format!("{name} is not installed"));
            }
            let extended = read(root, EXTENDED_STATES)?.unwrap_or_default();
            let extended = stanzas(&extended);
            if let Some(name) = request.packages.iter().find(|name| {
                extended
                    .get(name.as_str())
                    .is_some_and(|entries| entries.iter().any(|e| e.auto_installed))
            }) {
                return Err(format!(
                    "{name} is marked automatically installed, and apt-mark would change that"
                ));
            }
            let apt_mark = bin_path(task_env, "apt-mark")
                .ok_or("apt-mark is not on PATH, and the module would warn")?;
            cache(&mut result);
            Ok(Plan {
                result,
                mark: Some((apt_mark, request.packages)),
            })
        }
    }
}

/// Runs what the plan decided: nothing more for a cache check or `state: absent`,
/// `apt-mark manual` for `state: present`, the way the module's `mark_installed_manually` does,
/// under the task's `timeout` and cancel.
fn finish(
    plan: Plan,
    context: &Context,
    deadline: Option<Instant>,
    cancelled: &dyn Fn() -> bool,
) -> NativeRun {
    let Plan { mut result, mark } = plan;
    let Some((apt_mark, packages)) = mark else {
        return NativeRun::Done(TaskResult(result));
    };
    let timed_out = || {
        NativeRun::Done(TaskResult::timed_out(
            context.timeout.unwrap_or_default().as_secs(),
        ))
    };
    let mut env = context.environment.clone();
    // The module's `APT_ENV_VARS`, with the locale `get_best_parsable_locale` picks on a Debian
    // or Ubuntu host.
    for (key, value) in [
        ("DEBIAN_FRONTEND", "noninteractive"),
        ("DEBIAN_PRIORITY", "critical"),
        ("LANG", "C.UTF-8"),
        ("LC_ALL", "C.UTF-8"),
        ("LC_MESSAGES", "C.UTF-8"),
        ("LC_CTYPE", "C.UTF-8"),
        ("LANGUAGE", "C.UTF-8"),
    ] {
        env.insert(key.into(), value.into());
    }
    let program = apt_mark.to_string_lossy().into_owned();
    let mut first = true;
    let (cmd, rc, out, err) = loop {
        let op = if first { "manual" } else { "unmarkauto" };
        let mut argv = vec![program.clone(), op.to_string()];
        argv.extend(packages.iter().cloned());
        let (rc, out, err) = match execute(&argv, &env, deadline, cancelled) {
            Ok(Some(done)) => done,
            // Not started, so nothing changed yet: the module gets its chance.
            Ok(None) if first => {
                return NativeRun::Fallback("apt-mark could not be started".into());
            }
            Ok(None) => (-1, String::new(), "apt-mark could not be started".into()),
            Err(Stop::TimedOut) => return timed_out(),
            Err(Stop::Cancelled) => return NativeRun::Cancelled,
        };
        let cmd = format!("{program} {op} {}", packages.join(" "));
        if first && (err.contains(APT_MARK_INVALID_OP) || err.contains(APT_MARK_INVALID_OP_DEB6)) {
            first = false;
            continue;
        }
        break (cmd, rc, out, err);
    };
    if rc != 0 {
        for key in ["changed", "cache_updated", "cache_update_time"] {
            result.remove(key);
        }
        result.insert("failed".into(), true.into());
        result.insert("msg".into(), format!("'{cmd}' failed: {err}").into());
        result.insert("stdout".into(), out.into());
        result.insert("stderr".into(), err.into());
        result.insert("rc".into(), rc.into());
    }
    NativeRun::Done(TaskResult(result))
}

#[derive(Debug)]
enum Stop {
    TimedOut,
    Cancelled,
}

/// `module.run_command(argv)`, through the executor `command` runs under: exit code, standard
/// output and standard error, or `None` when the program could not be started.
fn execute(
    argv: &[String],
    env: &BTreeMap<String, String>,
    deadline: Option<Instant>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<(i64, String, String)>, Stop> {
    let timeout = match deadline {
        Some(deadline) => Some(
            deadline
                .checked_duration_since(Instant::now())
                .ok_or(Stop::TimedOut)?,
        ),
        None => None,
    };
    let mut command = Map::new();
    command.insert(
        "argv".into(),
        Value::Array(argv.iter().cloned().map(Value::from).collect()),
    );
    command.insert("strip_empty_ends".into(), false.into());
    let context = Context {
        timeout,
        environment: env.clone(),
        ..Context::default()
    };
    match crate::modules::command::execute(&command, false, &context, cancelled) {
        Run::Cancelled => Err(Stop::Cancelled),
        Run::Done(result) if result.0.contains_key("timedout") => Err(Stop::TimedOut),
        // Only a command that started has a `start`.
        Run::Done(result) if result.0.contains_key("start") => {
            let text = |key: &str| result.0[key].as_str().unwrap_or_default().to_string();
            Ok(Some((
                result.0["rc"].as_i64().unwrap_or(-1),
                text("stdout"),
                text("stderr"),
            )))
        }
        Run::Done(_) => Ok(None),
    }
}

/// The arguments checked against the spec and converted the way `AnsibleModule` converts
/// them. Anything the module would refuse, or would act on outside the fast path, is an `Err`:
/// the module then gives its own answer, failure messages included.
fn request(args: &Map<String, Value>) -> Result<Request, String> {
    for key in args.keys() {
        let arg = SPEC
            .iter()
            .find(|arg| arg.name == key || arg.aliases.contains(&key.as_str()))
            .ok_or_else(|| format!("argument '{key}' is outside the native apt"))?;
        let given = std::iter::once(arg.name)
            .chain(arg.aliases.iter().copied())
            .filter(|name| args.contains_key(*name))
            .count();
        if given > 1 {
            return Err(format!("{} is given under more than one name", arg.name));
        }
    }
    // `deb` and `upgrade` are mutually exclusive with `package`, and counted by presence.
    for key in ["deb", "upgrade"] {
        if args.contains_key(key) {
            return Err(format!("{key} is outside the native apt"));
        }
    }
    let value = |name: &str| -> Option<&Value> {
        let arg = SPEC.iter().find(|arg| arg.name == name)?;
        std::iter::once(arg.name)
            .chain(arg.aliases.iter().copied())
            .find_map(|name| args.get(name))
    };

    let mut module_args = match invocation(SPEC, args) {
        Value::Object(mut invocation) => match invocation.remove("module_args") {
            Some(Value::Object(module_args)) => module_args,
            _ => Map::new(),
        },
        _ => Map::new(),
    };

    let state = match value("state") {
        None => State::Present,
        Some(Value::String(state)) if state == "present" => State::Present,
        Some(Value::String(state)) if state == "absent" => State::Absent,
        Some(_) => return Err("a state other than present or absent".into()),
    };
    match value("default_release") {
        None | Some(Value::Null) => {}
        Some(Value::String(release)) if release.is_empty() => {}
        Some(_) => return Err("default_release is outside the native apt".into()),
    }

    for (name, take) in TAKEN {
        let Some(given) = value(name) else { continue };
        let converted = match (take, given) {
            (Take::OptBool | Take::OptInt, Value::Null) => Value::Null,
            (Take::Bool | Take::OptBool | Take::False, given) => {
                let b = to_bool(given).ok_or_else(|| format!("{name} is not a boolean"))?;
                if matches!(take, Take::False) && b {
                    return Err(format!("{name} is outside the native apt"));
                }
                b.into()
            }
            (Take::Int | Take::OptInt, given) => to_int(given)
                .ok_or_else(|| format!("{name} is not an integer"))?
                .into(),
            (Take::Str, Value::String(text)) => text.clone().into(),
            (Take::Str, _) => return Err(format!("{name} is not a string")),
        };
        module_args.insert((*name).to_string(), converted);
    }
    let update_cache = module_args["update_cache"].as_bool().unwrap_or(false);
    let cache_valid_time = match value("cache_valid_time") {
        None => 0,
        Some(given) => to_int(given).ok_or("cache_valid_time is not an integer")?,
    };
    // Past a billion seconds `datetime` arithmetic can overflow in the module.
    if !(0..=1_000_000_000).contains(&cache_valid_time) {
        return Err("cache_valid_time is out of the native range".into());
    }
    module_args.insert("cache_valid_time".into(), cache_valid_time.into());

    let packages = match value("package") {
        None => {
            if value("update_cache").is_none() {
                return Err("neither a package nor update_cache is given".into());
            }
            Vec::new()
        }
        // `type='list', elements='str'`: a string is split on commas.
        Some(Value::String(text)) => text.split(',').map(str::to_string).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or("a package name is not a string")?,
        Some(_) => return Err("package is not a list of names".into()),
    };
    if value("package").is_some() {
        if packages.is_empty() {
            return Err("the package list is empty".into());
        }
        if let Some(name) = packages.iter().find(|name| !is_exact_name(name)) {
            return Err(format!("'{name}' is not an exact package name"));
        }
        module_args.insert(
            "package".into(),
            Value::Array(packages.iter().cloned().map(Value::from).collect()),
        );
    }
    Ok(Request {
        state,
        packages,
        update_cache,
        cache_valid_time,
        module_args,
    })
}

/// A Debian package name and nothing else: no version (`=`, `>=`), no pattern (`*?[]!`), no
/// architecture (`:`), no space for the module to strip.
fn is_exact_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 2
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.')
        })
}

/// `check_type_bool`: a bool, or a string or number among the spellings it takes.
fn to_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => {
            let n = n.as_f64()?;
            (n == 1.0 || n == 0.0).then_some(n == 1.0)
        }
        Value::String(text) => match text.to_lowercase().trim() {
            "y" | "yes" | "on" | "1" | "true" | "t" => Some(true),
            "n" | "no" | "off" | "0" | "false" | "f" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `check_type_int`: an integer, or a string `int()` reads in plain ASCII digits.
fn to_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(text) => {
            let text = text.trim();
            let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            text.parse().ok()
        }
        _ => None,
    }
}

/// One paragraph of a dpkg-style file, the fields the fast path reads.
#[derive(Debug, Default)]
struct Stanza<'a> {
    status: &'a str,
    architecture: &'a str,
    auto_installed: bool,
}

impl Stanza<'_> {
    /// The third word of `Status:`, what `current_state` reports.
    fn current(&self) -> &str {
        self.status.split_whitespace().nth(2).unwrap_or_default()
    }
}

/// Every paragraph by its `Package:`.
fn stanzas(text: &str) -> HashMap<&str, Vec<Stanza<'_>>> {
    let mut map: HashMap<&str, Vec<Stanza>> = HashMap::new();
    for paragraph in text.split("\n\n") {
        let mut package = None;
        let mut stanza = Stanza::default();
        for line in paragraph.lines() {
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match field {
                "Package" => package = Some(value),
                "Status" => stanza.status = value,
                "Architecture" => stanza.architecture = value,
                "Auto-Installed" => stanza.auto_installed = value == "1",
                _ => {}
            }
        }
        if let Some(package) = package {
            map.entry(package).or_default().push(stanza);
        }
    }
    map
}

/// One paragraph for `name`, and it reads `install ok installed` (any other want or state,
/// `hold` and `deinstall` included, is left to the module), on `arch` or `all` when one is given.
fn is_installed(status: &HashMap<&str, Vec<Stanza>>, name: &str, arch: Option<&str>) -> bool {
    match status.get(name).map(Vec::as_slice) {
        Some([entry]) => {
            entry
                .status
                .split_whitespace()
                .eq(["install", "ok", "installed"])
                && arch.is_none_or(|arch| entry.architecture == arch || entry.architecture == "all")
        }
        _ => false,
    }
}

/// A file's text, `None` when it does not exist.
fn read(root: &Root, path: &str) -> Result<Option<String>, String> {
    match std::fs::read_to_string(root.path(path)) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{path}: {err}")),
    }
}

/// `get_cache_mtime`: the stamp's, else the lists directory's, else none.
fn cache_mtime(root: &Root) -> Result<Option<(i64, i64)>, String> {
    for path in [STAMP, LISTS] {
        match std::fs::metadata(root.path(path)) {
            Ok(meta) => return Ok(Some((meta.mtime(), meta.mtime_nsec()))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("{path}: {err}")),
        }
    }
    Ok(None)
}

/// `datetime.fromtimestamp(st_mtime)` as seconds and microseconds: `st_mtime` is the float
/// `sec + nsec * 1e-9`, and the microseconds are rounded half to even, carrying into the second.
/// `cache_update_time` is then that second (`mktime` of the tuple, which drops the microseconds):
/// neither the float's floor nor its rounding.
fn from_timestamp(sec: i64, nsec: i64) -> (i64, i64) {
    let float = sec as f64 + nsec as f64 * 1e-9;
    let whole = float.trunc();
    let micros = ((float - whole) * 1e6).round_ties_even();
    let carry = micros >= 1e6;
    let micros = if carry { micros - 1e6 } else { micros };
    (whole as i64 + i64::from(carry), micros as i64)
}

/// The local zone's offset from UTC at `seconds`, as the C library reads it.
fn utc_offset(seconds: i64) -> Option<libc::c_long> {
    let time = libc::time_t::try_from(seconds).ok()?;
    // SAFETY: `tm` is plain data, and `localtime_r` writes it whole or returns null.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let done = unsafe { libc::localtime_r(&raw const time, &raw mut tm) };
    (!done.is_null()).then_some(tm.tm_gmtoff)
}

/// `module.get_bin_path(name)`: the first executable file of that name along the module's
/// `PATH`, with `/sbin`, `/usr/sbin` and `/usr/local/sbin` added when they exist.
fn bin_path(task_env: &BTreeMap<String, String>, name: &str) -> Option<PathBuf> {
    let path = task_env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let mut dirs: Vec<&str> = path.split(':').collect();
    for sbin in ["/sbin", "/usr/sbin", "/usr/local/sbin"] {
        if !dirs.contains(&sbin) && Path::new(sbin).exists() {
            dirs.push(sbin);
        }
    }
    dirs.into_iter()
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|meta| !meta.is_dir() && meta.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use std::fs::{File, FileTimes};
    use std::time::Duration;

    use serde_json::json;

    use super::super::setup::tests::FakeRoot;
    use super::*;

    /// Seconds of the stamp the tests set; its fraction is `.7`, which a rounding would push up.
    const STAMP_SECONDS: u64 = 1_790_276_713;

    const STATUS_TEXT: &str = "\
Package: bash
Essential: yes
Status: install ok installed
Priority: required
Architecture: amd64
Version: 5.2.21-2ubuntu4
Description: GNU Bourne Again SHell
 Bash is an sh-compatible command language interpreter.

Package: coreutils
Status: install ok installed
Architecture: amd64

Package: dpkg
Status: install ok installed
Architecture: amd64

Package: python3-apt
Status: install ok installed
Architecture: amd64

Package: tzdata
Status: install ok installed
Architecture: all

Package: foo
Status: install ok half-configured
Architecture: amd64

Package: bar
Status: deinstall ok installed
Architecture: amd64

Package: held
Status: hold ok installed
Architecture: amd64

Package: gone
Status: deinstall ok config-files
Architecture: amd64

Package: libtwo
Status: install ok installed
Architecture: amd64

Package: libtwo
Status: install ok installed
Architecture: i386

Package: foreign
Status: install ok installed
Architecture: i386

Package: libfoo
Status: install ok installed
Architecture: amd64
";

    /// A host with the status above, a cache stamp at `STAMP_SECONDS.7`, an empty dpkg journal
    /// and an `apt-mark` on `PATH` that logs its arguments and exits 0.
    struct Host {
        fake: FakeRoot,
    }

    impl Host {
        fn new(name: &str) -> Host {
            let fake = FakeRoot::new(&format!("apt-{name}"));
            fake.write(STATUS, STATUS_TEXT)
                .write(
                    EXTENDED_STATES,
                    "Package: libfoo\nArchitecture: amd64\nAuto-Installed: 1\n",
                )
                .write(STAMP, "")
                .mkdir(UPDATES)
                .mkdir(LISTS);
            let host = Host { fake };
            host.stamp(STAMP_SECONDS, 700_000_000);
            host.apt_mark("exit 0");
            host
        }

        fn stamp(&self, seconds: u64, nanos: u32) {
            let file = File::options()
                .write(true)
                .open(self.fake.0.join(STAMP.trim_start_matches('/')))
                .unwrap();
            let when = UNIX_EPOCH + Duration::new(seconds, nanos);
            file.set_times(FileTimes::new().set_modified(when)).unwrap();
        }

        /// `apt-mark` on the fake `PATH`, logging `$*` then running `body`.
        fn apt_mark(&self, body: &str) {
            let log = self.log_path();
            self.fake.script(
                "/bin/apt-mark",
                &format!("echo \"$*\" >> '{}'\n{body}", log.display()),
            );
        }

        fn log_path(&self) -> PathBuf {
            self.fake.0.join("apt-mark.log")
        }

        /// What `apt-mark` was called with, one line per call.
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.log_path())
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn context(&self) -> Context {
            Context {
                environment: BTreeMap::from([(
                    "PATH".to_string(),
                    self.fake.0.join("bin").to_string_lossy().into_owned(),
                )]),
                ..Context::default()
            }
        }

        fn answer_at(&self, args: &Value, context: &Context, now: SystemTime) -> NativeRun {
            answer(
                args.as_object().unwrap(),
                &self.fake.root(),
                context,
                now,
                &|| false,
            )
        }

        /// The answer a minute after the stamp.
        fn answer(&self, args: &Value) -> NativeRun {
            self.answer_at(args, &self.context(), soon())
        }

        fn done(&self, args: &Value) -> Map<String, Value> {
            match self.answer(args) {
                NativeRun::Done(result) => result.0,
                NativeRun::Fallback(reason) => panic!("{args}: handed back, {reason}"),
                NativeRun::Cancelled => panic!("{args}: cancelled"),
            }
        }

        /// Hands back, and the host is as it was: no `apt-mark`, the status file untouched.
        fn hands_back(&self, args: &Value) -> String {
            let reason = match self.answer(args) {
                NativeRun::Fallback(reason) => reason,
                NativeRun::Done(result) => panic!("{args}: answered {:?}", result.0),
                NativeRun::Cancelled => panic!("{args}: cancelled"),
            };
            assert_eq!(self.calls(), Vec::<String>::new(), "{args}");
            reason
        }
    }

    fn soon() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(STAMP_SECONDS + 60)
    }

    /// The recording's answer less what the controller adds, and less `cache_update_time`, the
    /// generator's own cache date.
    fn recorded(name: &str) -> Map<String, Value> {
        let text = match name {
            "apt-update-fresh" => {
                include_str!("../../../volant/tests/golden/native/apt-update-fresh.json")
            }
            "apt-present-installed" => {
                include_str!("../../../volant/tests/golden/native/apt-present-installed.json")
            }
            "apt-present-list" => {
                include_str!("../../../volant/tests/golden/native/apt-present-list.json")
            }
            "apt-present-update-fresh" => {
                include_str!("../../../volant/tests/golden/native/apt-present-update-fresh.json")
            }
            "apt-absent-missing" => {
                include_str!("../../../volant/tests/golden/native/apt-absent-missing.json")
            }
            _ => unreachable!("{name}"),
        };
        let mut recording: Map<String, Value> = serde_json::from_str(text).unwrap();
        for key in ["_ansible_no_log", "action", "cache_update_time"] {
            recording.remove(key);
        }
        recording
    }

    /// The three answers, key by key against the golden recordings with the arguments the golden
    /// play gave: a fresh cache alone (A), packages all installed (B), with and without a cache
    /// check, and a package that is nowhere (C). `cache_update_time` is the stamp's second, and
    /// B runs `apt-mark manual` with the names, once per task, as the module does.
    ///
    /// What would make this red: any key the reference writes missing or different, the
    /// `invocation` included (`package` as a list, `upgrade: null`, the defaults); a
    /// `cache_update_time` for C, which the module's `remove` never adds; `apt-mark` skipped, or
    /// run for A or C.
    #[test]
    fn the_fast_path_answers_like_the_reference() {
        let host = Host::new("answers");
        for (name, args, marks) in [
            (
                "apt-update-fresh",
                json!({"cache_valid_time": 86400, "update_cache": true}),
                None,
            ),
            (
                "apt-present-installed",
                json!({"name": "bash", "state": "present"}),
                Some("manual bash"),
            ),
            (
                "apt-present-list",
                json!({"name": ["bash", "coreutils"], "state": "present"}),
                Some("manual bash coreutils"),
            ),
            (
                "apt-present-update-fresh",
                json!({"cache_valid_time": 86400, "name": "bash", "update_cache": true}),
                Some("manual bash"),
            ),
            (
                "apt-absent-missing",
                json!({"name": "volant-no-such-package", "state": "absent"}),
                None,
            ),
        ] {
            let _ = std::fs::remove_file(host.log_path());
            let mut ours = host.done(&args);
            let expected = recorded(name);
            if expected.contains_key("cache_updated") {
                assert_eq!(
                    ours.remove("cache_update_time"),
                    Some(json!(STAMP_SECONDS)),
                    "{name}"
                );
            }
            assert_eq!(Value::Object(ours), Value::Object(expected), "{name}");
            assert_eq!(
                host.calls(),
                marks.map(str::to_string).into_iter().collect::<Vec<_>>(),
                "{name}"
            );
        }
    }

    /// Review focus: a package dpkg left half configured is not installed, whatever the first
    /// and last words of its `Status:` say, and the module is the one to finish it.
    ///
    /// What would make this red: `Status:` read by its first or last word alone, or by
    /// `installed` anywhere in it, which answers "already present" for a package that does not
    /// work.
    #[test]
    fn a_half_configured_package_is_not_installed_for_the_fast_path() {
        let host = Host::new("half");
        let reason = host.hands_back(&json!({"name": "foo", "state": "present"}));
        assert_eq!(reason, "foo is not installed");
        host.hands_back(&json!({"name": ["bash", "foo"]}));
        // Removing it is not "nothing to do" either.
        host.hands_back(&json!({"name": "foo", "state": "absent"}));
    }

    /// A package marked for removal (`deinstall ok installed`) or on hold is left to the module,
    /// both ways.
    ///
    /// What would make this red: `Status:` read by its third word only, as python-apt's
    /// `current_state` does, which the native does not follow.
    #[test]
    fn a_package_not_wanted_as_installed_is_left_to_the_module() {
        let host = Host::new("deinstall");
        for name in ["bar", "held"] {
            host.hands_back(&json!({"name": name}));
            host.hands_back(&json!({"name": name, "state": "absent"}));
        }
    }

    /// A stale cache, or `update_cache` with no `cache_valid_time`, makes the module update: the
    /// native hands back for both, with or without packages.
    ///
    /// What would make this red: the cache's age checked only when `cache_valid_time` is given,
    /// or compared the wrong way.
    #[test]
    fn a_cache_the_module_would_update_is_left_to_it() {
        let host = Host::new("stale");
        assert_eq!(
            host.hands_back(&json!({"update_cache": true})),
            "update_cache without cache_valid_time"
        );
        for args in [
            json!({"update_cache": true, "name": "bash"}),
            json!({"update_cache": true, "cache_valid_time": 30, "name": "bash"}),
        ] {
            host.hands_back(&args);
        }
        let a_day_later = UNIX_EPOCH + Duration::from_secs(STAMP_SECONDS + 86_401);
        for args in [
            json!({"update_cache": true, "cache_valid_time": 86400}),
            json!({"cache_valid_time": 86400, "name": "volant-none", "state": "absent"}),
        ] {
            assert!(
                matches!(
                    host.answer_at(&args, &host.context(), a_day_later),
                    NativeRun::Fallback(_)
                ),
                "{args}"
            );
        }
        // At the edge, the module's `>=` still says fresh.
        let edge = UNIX_EPOCH + Duration::new(STAMP_SECONDS + 86_400, 700_000_000);
        assert!(matches!(
            host.answer_at(
                &json!({"update_cache": true, "cache_valid_time": 86400}),
                &host.context(),
                edge
            ),
            NativeRun::Done(_)
        ));
    }

    /// `cache_update_time` is the second of `datetime.fromtimestamp(st_mtime)`: the fraction
    /// dropped, except when it rounds to a whole second at the microsecond.
    ///
    /// What would make this red: the float rounded to the nearest second (`...714` for `.7`), or
    /// the nanoseconds truncated to microseconds before rounding (`...713` for `.9999996`).
    #[test]
    fn the_cache_time_is_the_module_s_second() {
        let host = Host::new("floor");
        let time = |host: &Host| {
            host.done(&json!({"update_cache": true, "cache_valid_time": 86400}))
                ["cache_update_time"]
                .clone()
        };
        assert_eq!(time(&host), json!(STAMP_SECONDS));
        host.stamp(STAMP_SECONDS, 999_999_600);
        assert_eq!(time(&host), json!(STAMP_SECONDS + 1));
        host.stamp(STAMP_SECONDS, 999_999_400);
        assert_eq!(time(&host), json!(STAMP_SECONDS));
        // No stamp: the lists directory's time.
        std::fs::remove_file(host.fake.0.join(STAMP.trim_start_matches('/'))).unwrap();
        let lists = File::open(host.fake.0.join(LISTS.trim_start_matches('/'))).unwrap();
        lists
            .set_times(
                FileTimes::new().set_modified(UNIX_EPOCH + Duration::new(STAMP_SECONDS - 5, 1)),
            )
            .unwrap();
        assert_eq!(time(&host), json!(STAMP_SECONDS - 5));
    }

    /// `apt-mark manual` runs with the names, and its failure is the module's: the command line,
    /// standard error, output and exit code, in the shape measured on ansible-core 2.19.12 with
    /// an `apt-mark` exiting 3. An `apt-mark` too old for `manual` gets `unmarkauto`.
    ///
    /// What would make this red: `apt-mark` not run, or run with other arguments; its failure
    /// reported as success; the retry missing.
    #[test]
    fn apt_mark_runs_as_the_module_runs_it() {
        let host = Host::new("mark");
        host.apt_mark("echo out-line; echo \"E: boom $*\" >&2; exit 3");
        let result = host.done(&json!({"name": "bash"}));
        let program = host.fake.0.join("bin/apt-mark").display().to_string();
        assert_eq!(result["failed"], json!(true));
        assert_eq!(
            result["msg"],
            json!(format!(
                "'{program} manual bash' failed: E: boom manual bash\n"
            ))
        );
        assert_eq!(result["stdout"], json!("out-line\n"));
        assert_eq!(result["stderr"], json!("E: boom manual bash\n"));
        assert_eq!(result["rc"], json!(3));
        assert!(result.contains_key("invocation"));
        assert!(!result.contains_key("cache_update_time"));

        let _ = std::fs::remove_file(host.log_path());
        host.apt_mark(
            "[ \"$1\" = manual ] && { echo 'E: Invalid operation manual' >&2; exit 100; }; exit 0",
        );
        let result = host.done(&json!({"name": ["bash", "tzdata"]}));
        assert_eq!(result.get("failed"), None, "{result:?}");
        assert_eq!(
            host.calls(),
            ["manual bash tzdata", "unmarkauto bash tzdata"]
        );
    }

    /// A task's `timeout` reaches `apt-mark`, which is killed; a cancel stops it too.
    ///
    /// What would make this red: `apt-mark` run outside the executor, which waits for it
    /// however long it hangs.
    #[test]
    fn a_hung_apt_mark_times_out_or_is_cancelled() {
        let host = Host::new("hung");
        host.apt_mark("exec /bin/sleep 60");
        let mut context = host.context();
        context.timeout = Some(Duration::from_secs(1));
        let started = Instant::now();
        let NativeRun::Done(result) = host.answer_at(&json!({"name": "bash"}), &context, soon())
        else {
            panic!("not an answer");
        };
        assert_eq!(result.0, TaskResult::timed_out(1).0);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );

        let started = Instant::now();
        let asked = std::cell::Cell::new(0);
        let run = answer(
            json!({"name": "bash"}).as_object().unwrap(),
            &host.fake.root(),
            &host.context(),
            soon(),
            &|| {
                asked.set(asked.get() + 1);
                asked.get() > 2
            },
        );
        assert!(matches!(run, NativeRun::Cancelled));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    /// Everything outside the three answers goes to the module untouched.
    ///
    /// What would make this red: any of these answered, each one a case where the module
    /// installs, removes, updates, refuses, warns or reads something the native does not.
    #[test]
    fn everything_else_is_handed_back_before_any_change() {
        let host = Host::new("outside");
        for args in [
            json!({"name": "bash=5.2.21-2ubuntu4"}),
            json!({"name": "bash>=5"}),
            json!({"name": "bas*"}),
            json!({"name": "bash:amd64"}),
            json!({"name": " bash"}),
            json!({"name": "Bash"}),
            json!({"name": []}),
            json!({"name": [1]}),
            json!({"name": "bash", "state": "latest"}),
            json!({"name": "bash", "state": "fixed"}),
            json!({"name": "bash", "state": "bogus"}),
            json!({"deb": "/tmp/x.deb"}),
            json!({"name": "bash", "default_release": "noble"}),
            json!({"name": "gone", "state": "absent", "purge": true}),
            json!({"name": "bash", "autoremove": true}),
            json!({"name": "bash", "autoclean": true}),
            json!({"upgrade": "dist"}),
            json!({"name": "bash", "upgrade": "no"}),
            json!({"name": "bash", "only_upgrade": true}),
            json!({"name": "bash", "clean": true}),
            json!({"name": "bash", "force": "maybe"}),
            json!({"name": "bash", "lock_timeout": "soon"}),
            json!({"name": "bash", "lock_timeout": 1.5}),
            json!({"name": "bash", "dpkg_options": 3}),
            json!({"name": "bash", "purge": null}),
            json!({"name": "bash", "no_such": 1}),
            json!({"name": "bash", "_ansible_check_mode": true}),
            json!({"name": "bash", "pkg": "bash"}),
            json!({"cache_valid_time": 86400}),
            json!({"update_cache": false}),
            json!({"update_cache": true, "cache_valid_time": -1}),
            json!({"update_cache": true, "cache_valid_time": "1_000"}),
            // Not in the status file: unknown, or virtual.
            json!({"name": "volant-no-such-package"}),
            json!({"name": "mail-transport-agent"}),
            // Two architectures installed, or only a foreign one.
            json!({"name": "libtwo"}),
            json!({"name": "foreign"}),
            // `apt-mark manual` would change what apt believes.
            json!({"name": "libfoo"}),
        ] {
            host.hands_back(&args);
        }
        let mut context = host.context();
        context
            .environment
            .insert("TZ".into(), "Europe/Paris".into());
        assert!(matches!(
            host.answer_at(&json!({"name": "bash"}), &context, soon()),
            NativeRun::Fallback(_)
        ));
    }

    /// Host states the module reads differently from the status file alone: a dpkg journal not
    /// yet folded in, no python-apt (the module would install it), no `apt-mark` (it would warn).
    ///
    /// What would make this red: any of them answered.
    #[test]
    fn a_host_the_status_file_does_not_describe_is_left_to_the_module() {
        let host = Host::new("journal");
        host.fake
            .write("/var/lib/dpkg/updates/0001", "Package: bash\n");
        assert_eq!(
            host.hands_back(&json!({"name": "bash"})),
            "dpkg has pending updates in its journal"
        );

        let host = Host::new("no-python-apt");
        host.fake.write(
            STATUS,
            &STATUS_TEXT.replace("Package: python3-apt\n", "Package: python3-apx\n"),
        );
        let reason = host.hands_back(&json!({"name": "bash"}));
        assert!(reason.starts_with("python3-apt"), "{reason}");

        let host = Host::new("no-apt-mark");
        let mut context = host.context();
        context.environment.insert(
            "PATH".into(),
            host.fake.0.join("nowhere").display().to_string(),
        );
        assert!(matches!(
            host.answer_at(&json!({"name": "bash"}), &context, soon()),
            NativeRun::Fallback(_)
        ));
    }

    /// Arguments as a `key=value` line or a template leaves them, strings, read the way
    /// `AnsibleModule` converts them, and shown converted in `invocation`; an alias keeps the
    /// value it was given beside the converted one.
    ///
    /// What would make this red: `"yes"` or `"86400"` refused (every `apt: update_cache=yes
    /// cache_valid_time=86400` would go to Python), or passed through unconverted.
    #[test]
    fn string_arguments_are_converted_like_the_module_converts_them() {
        let host = Host::new("strings");
        let result = host.done(&json!({"update-cache": "Yes ", "cache_valid_time": " 86400"}));
        let args = &result["invocation"]["module_args"];
        assert_eq!(args["update_cache"], json!(true));
        assert_eq!(args["update-cache"], json!("Yes "));
        assert_eq!(args["cache_valid_time"], json!(86400));
        let result = host.done(&json!({
            "pkg": "bash,coreutils",
            "state": "present",
            "install_recommends": "no",
            "lock_timeout": "30",
            "default_release": "",
            "force": 0,
        }));
        let args = &result["invocation"]["module_args"];
        assert_eq!(args["package"], json!(["bash", "coreutils"]));
        assert_eq!(args["pkg"], json!("bash,coreutils"));
        assert_eq!(args["install_recommends"], json!(false));
        assert_eq!(args["lock_timeout"], json!(30));
        assert_eq!(args["force"], json!(false));
        assert_eq!(args["default_release"], json!(""));
        assert_eq!(args["upgrade"], Value::Null);
    }
}
