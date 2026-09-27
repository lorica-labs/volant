// SPDX-License-Identifier: GPL-3.0-or-later
//! `apt`, answered in the agent when the task asks for what the host already has: a package
//! cache that is still fresh, packages that are all installed, or packages none of which is.
//! Every other task goes to the Python module, handed back before anything on the host changed.
//!
//! Read from ansible-core 2.19.12's `apt.py`, whose answers in those three cases are:
//!
//! - no package, and a fresh cache or no cache check at all:
//!   `{"changed": false, "cache_updated": false, "cache_update_time": T}` (`{"changed": false}`
//!   for `state: absent` without a check);
//! - `state: present`, every package `install ok installed` and marked manually installed: the
//!   same three keys;
//! - `state: absent`, no package installed: `{"changed": false}`.
//!
//! One divergence: for `state: present` the module runs `apt-mark manual <packages>` whether or
//! not anything changes. With every name already manual that call is a no-op, and the native
//! does not spawn it: an `apt-mark` that would fail there without changing anything is not
//! reported. A name marked automatically installed, where the call changes something, goes to
//! the module.
//!
//! The native reads files only (`/var/lib/dpkg/status`, `/var/lib/apt/extended_states`, the
//! cache stamp) and takes no lock. dpkg replaces its status file by renaming a complete new one
//! over it, so a read sees one version or the other, never a mix; a journal left in
//! `/var/lib/dpkg/updates/` means the status file is not the whole story, and hands back.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use super::common::{ArgSpec, invocation};
use super::setup::Root;
use super::{Native, NativeRun};
use crate::modules::Context;

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

/// Reads files only and spawns nothing, so there is no `timeout` or cancel to honour.
fn run(args: &Map<String, Value>, context: &Context, _: &dyn Fn() -> bool) -> NativeRun {
    answer(args, &Root::real(), context, SystemTime::now())
}

fn answer(args: &Map<String, Value>, root: &Root, context: &Context, now: SystemTime) -> NativeRun {
    match plan(args, root, &context.environment, now) {
        Err(reason) => NativeRun::Fallback(reason),
        Ok(result) => NativeRun::Done(TaskResult(result)),
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

/// Decides, reading only. `Err` is the reason the task goes to the Python module.
fn plan(
    args: &Map<String, Value>,
    root: &Root,
    task_env: &BTreeMap<String, String>,
    now: SystemTime,
) -> Result<Map<String, Value>, String> {
    if task_env.contains_key("TZ") {
        return Err("the task sets TZ, which moves the module's local time".into());
    }
    let request = request(args)?;
    let status = read(root, STATUS)?.ok_or("the dpkg status file is missing")?;
    let status = stanzas(&status);
    if !is_installed(&status, "python3-apt", None) || !system_python_has_apt(root) {
        return Err(
            "python3-apt is not importable by the system python, and the module would install it"
                .into(),
        );
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
    // The module makes `cache_update_time` with `mktime`, and compares the cache's age in naive
    // local times: each equals its UTC counterpart only while the zone keeps one offset around
    // the stamp, and, for the age, from it to now.
    let check = request.update_cache || request.cache_valid_time != 0;
    let mut instants = vec![seconds - 3600, seconds, seconds + 3600];
    if check {
        instants.push(now_seconds);
    }
    let offsets: Option<Vec<libc::c_long>> = instants.into_iter().map(utc_offset).collect();
    if !offsets.is_some_and(|o| o.windows(2).all(|w| w[0] == w[1])) {
        return Err("the local time changes offset around the cache stamp".into());
    }
    if check {
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
    // No package: the module exits after its cache check, or, without one, installs nothing
    // (the cache keys) or removes nothing (`changed` alone).
    if request.packages.is_empty() {
        if check || request.state == State::Present {
            cache(&mut result);
        } else {
            result.insert("changed".into(), false.into());
        }
        return Ok(result);
    }
    match request.state {
        State::Absent => {
            if request.packages.iter().any(|name| {
                status.get(name.as_str()).is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|e| !matches!(e.current(), "not-installed" | "config-files"))
                })
            }) {
                return Err("a package is installed, or partly".into());
            }
            result.insert("changed".into(), false.into());
            Ok(result)
        }
        State::Present => {
            // `dpkg` is always installed for the native architecture.
            let native_arch = match status.get("dpkg").map(Vec::as_slice) {
                Some([dpkg]) => dpkg.architecture,
                _ => return Err("the native architecture is not dpkg's alone".into()),
            };
            if request
                .packages
                .iter()
                .any(|name| !is_installed(&status, name, Some(native_arch)))
            {
                return Err("a package is not installed".into());
            }
            // The module's `apt-mark manual` would change what apt believes: that task is its own.
            let extended = read(root, EXTENDED_STATES)?.unwrap_or_default();
            let extended = stanzas(&extended);
            if request.packages.iter().any(|name| {
                extended
                    .get(name.as_str())
                    .is_some_and(|entries| entries.iter().any(|e| e.auto_installed))
            }) {
                return Err(
                    "a package is marked automatically installed, and apt-mark would change that"
                        .into(),
                );
            }
            // Without `apt-mark` the module warns; with it, every name manual, the call it makes
            // is a no-op the native skips.
            if bin_path(task_env, "apt-mark").is_none() {
                return Err("apt-mark is not on PATH, and the module would warn".into());
            }
            cache(&mut result);
            Ok(result)
        }
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
        // `required_one_of` always holds: `autoremove` gets its default before it is checked.
        None => Vec::new(),
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
        if !packages.iter().all(|name| is_exact_name(name)) {
            return Err("a package is not given by its exact name".into());
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

/// Whether `/usr/bin/python3` (or `/usr/bin/python`) can import python-apt, which is where the
/// module goes when its own interpreter cannot: a `python3.N` whose `apt_pkg` extension for that
/// version is in `dist-packages`. A system python of another version (the known
/// `No module named 'apt_pkg'` breakage), or one this cannot name, is not.
fn system_python_has_apt(root: &Root) -> bool {
    let packages = root.path("/usr/lib/python3/dist-packages");
    if !packages.join("apt/__init__.py").is_file() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(&packages) else {
        return false;
    };
    let extensions: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("apt_pkg.cpython-3") && name.ends_with(".so"))
        .collect();
    ["/usr/bin/python3", "/usr/bin/python"]
        .iter()
        .any(|python| {
            let Ok(real) = std::fs::canonicalize(root.path(python)) else {
                return false;
            };
            let Some(minor) = real
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("python3."))
                .filter(|minor| !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()))
            else {
                return false;
            };
            let tag = format!("apt_pkg.cpython-3{minor}-");
            extensions.iter().any(|name| name.starts_with(&tag))
        })
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
#[cfg_attr(
    target_env = "musl",
    expect(
        deprecated,
        reason = "libc marks `time_t` deprecated on musl ahead of its 64-bit change"
    )
)]
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

#[cfg(all(test, target_os = "linux"))]
pub(super) mod tests {
    use std::fs::{File, FileTimes};
    use std::time::Duration;

    use serde_json::json;

    use super::super::setup::tests::FakeRoot;
    use super::*;

    /// The reason guard's cases (`natives::tests`): a package with a secret-looking name,
    /// installed and marked automatically installed, removed, kept, asked for and misnamed.
    pub(in crate::natives) fn secret_probes() -> Vec<crate::natives::tests::Probe> {
        let host = Host::new("secret");
        host.fake
            .write(
                STATUS,
                &format!(
                    "{STATUS_TEXT}\nPackage: k3s0secret0r\nStatus: install ok installed\n\
                     Architecture: amd64\nVersion: 1\n"
                ),
            )
            .write(
                EXTENDED_STATES,
                "Package: k3s0secret0r\nArchitecture: amd64\nAuto-Installed: 1\n",
            );
        let reason = |args: Value| match host.answer(&args) {
            NativeRun::Fallback(reason) => Some(reason),
            _ => None,
        };
        vec![
            (
                "apt absent",
                "k3s0secret0r",
                reason(json!({"name": "k3s0secret0r", "state": "absent"})),
            ),
            (
                "apt auto-installed",
                "k3s0secret0r",
                reason(json!({"name": "k3s0secret0r"})),
            ),
            (
                "apt not installed",
                "k3s0secret0s",
                reason(json!({"name": "k3s0secret0s"})),
            ),
            (
                "apt inexact name",
                "k3s0secret0t",
                reason(json!({"name": ["k3s0secret0t!"]})),
            ),
        ]
    }

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
                .mkdir(LISTS)
                .write("/usr/lib/python3/dist-packages/apt/__init__.py", "")
                .write(
                    "/usr/lib/python3/dist-packages/apt_pkg.cpython-312-x86_64-linux-gnu.so",
                    "",
                )
                .write("/usr/bin/python3.12", "");
            std::os::unix::fs::symlink("python3.12", fake.0.join("usr/bin/python3")).unwrap();
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
            answer(args.as_object().unwrap(), &self.fake.root(), context, now)
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
    /// no `apt-mark` is spawned: every name is manual, where the module's call is a no-op.
    ///
    /// What would make this red: any key the reference writes missing or different, the
    /// `invocation` included (`package` as a list, `upgrade: null`, the defaults); a
    /// `cache_update_time` for C, which the module's `remove` never adds; `apt-mark` spawned.
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
                None,
            ),
            (
                "apt-present-list",
                json!({"name": ["bash", "coreutils"], "state": "present"}),
                None,
            ),
            (
                "apt-present-update-fresh",
                json!({"cache_valid_time": 86400, "name": "bash", "update_cache": true}),
                None,
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
        assert_eq!(reason, "a package is not installed");
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
            json!({"update_cache": true, "cache_valid_time": -1}),
            json!({"update_cache": true, "cache_valid_time": "1_000"}),
            // Not in the status file: unknown, or virtual.
            json!({"name": "volant-no-such-package"}),
            json!({"name": "mail-transport-agent"}),
            // Two architectures installed, or only a foreign one.
            json!({"name": "libtwo"}),
            json!({"name": "foreign"}),
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

        // dpkg has python3-apt, built for 3.12, and the system python is now a 3.13.
        let host = Host::new("other-python");
        host.fake.write("/usr/bin/python3.13", "");
        std::fs::remove_file(host.fake.0.join("usr/bin/python3")).unwrap();
        std::os::unix::fs::symlink("python3.13", host.fake.0.join("usr/bin/python3")).unwrap();
        let reason = host.hands_back(&json!({"name": "bash"}));
        assert!(reason.starts_with("python3-apt"), "{reason}");
        // The reference tries `/usr/bin/python` next.
        std::os::unix::fs::symlink("python3.12", host.fake.0.join("usr/bin/python")).unwrap();
        host.done(&json!({"name": "bash"}));

        // The extension without the package around it.
        let host = Host::new("no-apt-package");
        std::fs::remove_file(
            host.fake
                .0
                .join("usr/lib/python3/dist-packages/apt/__init__.py"),
        )
        .unwrap();
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

    /// A name marked automatically installed is the module's: its `apt-mark manual` changes what
    /// apt believes, and no golden case holds the native to doing that. Every name manual: the
    /// answer, and no `apt-mark` spawned (the fake records every call).
    ///
    /// What would make this red: the auto-installed name answered (with or without the call), or
    /// `apt-mark` spawned on the all-manual path.
    #[test]
    fn an_auto_installed_name_goes_to_the_module_and_manual_ones_spawn_nothing() {
        let host = Host::new("manual");
        let reason = host.hands_back(&json!({"name": ["bash", "libfoo"]}));
        assert!(
            reason.starts_with("a package is marked automatically"),
            "{reason}"
        );
        host.done(&json!({"name": ["bash", "coreutils", "tzdata"]}));
        assert_eq!(host.calls(), Vec::<String>::new());
    }

    /// With no package the module answers from its cache check alone, or, without one, from
    /// installing nothing (the cache keys) or removing nothing (`changed` alone). `required_one_of`
    /// never stops it: `autoremove` has its default before the check.
    ///
    /// What would make this red: `cache_valid_time` alone or `update_cache: false` alone handed
    /// back, or answered with the wrong keys for `state: absent`.
    #[test]
    fn a_task_with_no_package_is_answered_like_the_module() {
        let host = Host::new("no-package");
        let three = |result: &Map<String, Value>| {
            (
                result.get("changed").cloned(),
                result.get("cache_updated").cloned(),
                result.get("cache_update_time").cloned(),
            )
        };
        let expected = (
            Some(json!(false)),
            Some(json!(false)),
            Some(json!(STAMP_SECONDS)),
        );
        for args in [
            json!({"cache_valid_time": 86400}),
            json!({"cache_valid_time": "86400", "update_cache": false}),
            json!({"update_cache": false}),
            json!({}),
            json!({"cache_valid_time": 86400, "state": "absent"}),
        ] {
            assert_eq!(three(&host.done(&args)), expected, "{args}");
        }
        let result = host.done(&json!({"state": "absent"}));
        assert_eq!(three(&result), (Some(json!(false)), None, None));
        assert_eq!(host.calls(), Vec::<String>::new());
        let a_day_later = UNIX_EPOCH + Duration::from_secs(STAMP_SECONDS + 86_401);
        assert!(matches!(
            host.answer_at(
                &json!({"cache_valid_time": 86400}),
                &host.context(),
                a_day_later
            ),
            NativeRun::Fallback(_)
        ));
    }

    /// The zone's offset between the stamp and now matters only to a cache check: a stamp from
    /// summer time read in winter still gives `cache_update_time` for a task without one.
    ///
    /// Needs the `Europe/Paris` zone in the system's tz database. The test runs itself again in
    /// a child process that has `TZ` set, which leaves this process's environment alone.
    ///
    /// What would make this red: `now` compared on every task, which hands this one back; or not
    /// compared with a check, which answers an age the module measures an hour differently.
    #[test]
    fn only_a_cache_check_needs_the_same_offset_now() {
        // The marker, not `TZ` itself, tells the child it is one: a child that did not get `TZ`
        // fails below rather than starting another.
        if std::env::var_os("VOLANT_TZ_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "natives::apt::tests::only_a_cache_check_needs_the_same_offset_now",
                ])
                .env("TZ", "Europe/Paris")
                .env("VOLANT_TZ_CHILD", "1")
                .status()
                .unwrap();
            assert!(status.success(), "the test failed under TZ=Europe/Paris");
            return;
        }
        let summer = 1_790_276_713_i64; // 2026-09-24, CEST
        let winter = summer + 45 * 86_400; // 2026-11-08, CET
        assert_ne!(
            utc_offset(summer),
            utc_offset(winter),
            "this test needs the Europe/Paris zone in the system's tz database"
        );
        let host = Host::new("offset");
        let later = UNIX_EPOCH + Duration::from_secs(winter.unsigned_abs());
        let NativeRun::Done(result) =
            host.answer_at(&json!({"name": "bash"}), &host.context(), later)
        else {
            panic!("a task without a cache check handed back");
        };
        assert_eq!(result.0["cache_update_time"], json!(STAMP_SECONDS));
        assert!(matches!(
            host.answer_at(
                &json!({"name": "bash", "cache_valid_time": 100_000_000}),
                &host.context(),
                later
            ),
            NativeRun::Fallback(_)
        ));
    }
}
