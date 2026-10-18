// SPDX-License-Identifier: GPL-3.0-or-later
//! `package_facts`, answered in the agent for apt: what ansible-core 2.19.12 reads through
//! python-apt, read from the files python-apt's cache is built from.
//!
//! The reference lists every package whose `current_ver` is set and gives each installed version's
//! name, version, architecture, section and `origins[0].origin`. apt builds its cache from the
//! lists of the configured sources first and from dpkg's `status` last: a version found in a list
//! is the list's record (its section, and the list's `Release` for its first origin), and the
//! status only adds itself to it when the fields apt hashes match. A version no list carries is the
//! status's own record, whose origin is the empty string.
//!
//! The native answers only where that reading is certain, and hands back otherwise, before
//! anything runs: a package in a transitional state (`half-configured`, `unpacked`, ...), a
//! foreign architecture, a list no configured source names, lists that disagree about one
//! version, an apt configuration that moves its files, a manager or strategy other than apt
//! first. Collecting changes nothing on the host, so a hand-back is always safe.

use super::Native;
#[cfg(not(unix))]
use super::NativeRun;

#[cfg(unix)]
pub const NATIVE: Native = Native {
    name: "package_facts",
    aliases: &[],
    enabled: true,
    run: imp::run,
};

/// The agent is only ever uploaded to Linux hosts; elsewhere the task goes to the Python module.
#[cfg(not(unix))]
pub const NATIVE: Native = Native {
    name: "package_facts",
    aliases: &[],
    enabled: false,
    run: |_, _, _| NativeRun::Fallback("the native package_facts reads a Linux host".into()),
};

/// The reason guard's cases, for `natives::tests`.
#[cfg(all(test, target_os = "linux"))]
pub(super) use tests::secret_probes;

#[cfg(unix)]
mod dpkg;
#[cfg(unix)]
mod scan;
#[cfg(unix)]
mod sources;
#[cfg(all(test, unix))]
mod tests;

#[cfg(unix)]
mod imp {
    use std::collections::{BTreeMap, BTreeSet};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Instant;

    use serde_json::{Map, Value};
    use volant_protocol::TaskResult;

    use super::dpkg::installed;
    use super::scan::{record, scan, scan_all};
    use super::sources::{origin, sources};
    use crate::modules::Context;
    use crate::natives::NativeRun;
    use crate::natives::runtime::{Clock, Root, Stop};

    const STATUS: &str = "/var/lib/dpkg/status";
    pub(super) const LISTS: &str = "/var/lib/apt/lists";

    pub fn run(
        args: &Map<String, Value>,
        context: &Context,
        cancelled: &dyn Fn() -> bool,
    ) -> NativeRun {
        let clock = Clock {
            deadline: context.timeout.map(|timeout| Instant::now() + timeout),
            cancelled,
        };
        match answer(args, &Root::real(), &module_env(context), clock) {
            Ok(result) => NativeRun::Done(TaskResult(result)),
            Err(Stop::HandBack(reason)) => NativeRun::Fallback(reason),
            // What the Python path answers when the module outlives the task's `timeout`.
            Err(Stop::TimedOut) => NativeRun::Done(TaskResult::timed_out(
                context.timeout.unwrap_or_default().as_secs(),
            )),
            Err(Stop::Cancelled) => NativeRun::Cancelled,
        }
    }

    /// The environment the module runs with: the agent's, plus the task's.
    pub(super) fn module_env(context: &Context) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .collect();
        env.extend(context.environment.clone());
        env
    }

    /// The arguments, once validated as `AnsibleModule` would: `manager` as the list it converts
    /// to, and whether `apt` is named, which puts it before `apk` in the order `auto` tries.
    #[derive(Debug, PartialEq)]
    pub(super) struct Request {
        pub(super) manager: Vec<String>,
        pub(super) apt_named: bool,
    }

    /// The subset: `manager` naming apt and at most one `auto`, `strategy: first`. Anything else,
    /// a wrong type or value included, goes to the module, which answers it in its own words.
    pub(super) fn request(args: &Map<String, Value>) -> Result<Request, String> {
        if let Some(key) = args
            .keys()
            .find(|key| *key != "manager" && *key != "strategy")
        {
            return Err(format!(
                "the argument {key} is outside the native package_facts"
            ));
        }
        let manager: Vec<String> = match args.get("manager") {
            None => vec!["auto".into()],
            // `check_type_list` splits a string on commas.
            Some(Value::String(text)) => text.split(',').map(str::to_string).collect(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| item.as_str().map(str::to_string))
                .collect::<Option<_>>()
                .ok_or("manager holds a value that is not a string")?,
            Some(_) => return Err("manager is neither a list nor a string".into()),
        };
        let autos = manager
            .iter()
            .filter(|name| name.eq_ignore_ascii_case("auto"))
            .count();
        let apts = manager
            .iter()
            .filter(|name| name.eq_ignore_ascii_case("apt"))
            .count();
        // Two `auto`s leave one in the list after the module removes the first, which it then
        // refuses as an unsupported manager.
        if manager.is_empty() || autos + apts != manager.len() || autos > 1 {
            return Err("manager names something other than apt and auto".into());
        }
        match args.get("strategy") {
            None => {}
            Some(Value::String(strategy)) if strategy == "first" => {}
            Some(_) => return Err("strategy is not first".into()),
        }
        Ok(Request {
            manager,
            apt_named: apts > 0,
        })
    }

    pub(super) fn answer(
        args: &Map<String, Value>,
        root: &Root,
        env: &BTreeMap<String, String>,
        clock: Clock,
    ) -> Result<Map<String, Value>, Stop> {
        let request = request(args)?;
        if env.contains_key("APT_CONFIG") {
            return Err("APT_CONFIG gives apt another configuration".into());
        }
        // `auto` tries the managers in name order, and `apk` sorts before `apt`.
        if !request.apt_named && bin_path(root, env, "apk")?.is_some() {
            return Err("apk is found, and auto tries it before apt".into());
        }
        apt_config_gate(root)?;
        let updates = std::fs::read_dir(root.path("/var/lib/dpkg/updates"))
            .is_ok_and(|mut dir| dir.next().is_some());
        if updates {
            return Err("dpkg has updates it has not written to its status".into());
        }
        let status = read_text(root, STATUS)?;
        let (native, installed) = installed(&status)?;
        if !installed.contains_key("python3-apt") || !root.is_file("/usr/bin/python3") {
            return Err("python3-apt is not installed for /usr/bin/python3".into());
        }
        let foreign: Vec<String> = root
            .lines("/var/lib/dpkg/arch")
            .into_iter()
            .filter(|arch| *arch != native)
            .collect();
        let sources = sources(root, native, &foreign)?;
        let expected: BTreeSet<&str> = sources
            .iter()
            .flat_map(|source| source.packages.iter().map(String::as_str))
            .collect();
        let dir = std::fs::read_dir(root.path(LISTS))
            .map_err(|err| format!("{LISTS} cannot be read: {err}"))?;
        for entry in dir {
            let entry = entry.map_err(|err| format!("{LISTS} cannot be read: {err}"))?;
            let name = entry.file_name();
            let name = name.to_str().ok_or("a list's name is not UTF-8")?;
            if name.contains("_Packages.") {
                return Err(format!("{name} is a compressed list").into());
            }
            if name.ends_with("_Packages") && !expected.contains(name) {
                return Err(format!("{name} is a list no configured source names").into());
            }
        }
        let mut files = Vec::new();
        for source in &sources {
            let present: Vec<PathBuf> = source
                .packages
                .iter()
                .map(|name| root.path(&format!("{LISTS}/{name}")))
                .filter(|path| path.is_file())
                .collect();
            if present.is_empty() {
                continue;
            }
            let origin = origin(root, &source.prefix)?;
            files.extend(present.into_iter().map(|path| (path, origin.clone())));
        }
        let found = scan_all(&files, &installed, clock, scan)?;
        let mut packages = Map::new();
        for (name, package) in &installed {
            let record = record(
                name,
                package,
                found.get(*name).map_or(&[][..], Vec::as_slice),
            )?;
            let mut details = Map::new();
            details.insert("name".into(), Value::from(*name));
            details.insert("version".into(), Value::from(package.version));
            details.insert("arch".into(), Value::from(package.arch));
            details.insert("category".into(), Value::from(record.0));
            details.insert("origin".into(), Value::from(record.1));
            details.insert("source".into(), Value::from("apt"));
            packages.insert(
                (*name).to_string(),
                Value::Array(vec![Value::Object(details)]),
            );
        }
        let mut facts = Map::new();
        facts.insert("packages".into(), Value::Object(packages));
        let mut module_args = Map::new();
        module_args.insert("manager".into(), Value::from(request.manager));
        module_args.insert("strategy".into(), Value::from("first"));
        let mut invocation = Map::new();
        invocation.insert("module_args".into(), Value::Object(module_args));
        let mut result = Map::new();
        result.insert("ansible_facts".into(), Value::Object(facts));
        result.insert("invocation".into(), Value::Object(invocation));
        Ok(result)
    }

    pub(super) fn read_text(root: &Root, path: &str) -> Result<String, String> {
        let bytes = std::fs::read(root.path(path))
            .map_err(|err| format!("{path} cannot be read: {err}"))?;
        String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8"))
    }

    /// `get_bin_path(name)`: the first executable file of that name along `PATH`, with `/sbin`,
    /// `/usr/sbin` and `/usr/local/sbin` added when missing. A relative `PATH` entry depends on
    /// the module's working directory, which the native does not share: it hands back.
    fn bin_path(
        root: &Root,
        env: &BTreeMap<String, String>,
        name: &str,
    ) -> Result<Option<PathBuf>, String> {
        let mut dirs: Vec<&str> = env
            .get("PATH")
            .map_or("", String::as_str)
            .split(':')
            .collect();
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

    /// Hands back when apt's configuration could move the files read here or change the
    /// architectures it lists: `Dir` and its children, `APT::Architecture(s)`, in the `::` form
    /// or inside a block, and the includes that could hide them. Read without whitespace or
    /// case, so a match can also be a false alarm, which only costs the hand-back.
    fn apt_config_gate(root: &Root) -> Result<(), String> {
        const NEEDLES: [&str; 9] = [
            "dir::state",
            "dir::etc",
            "dir\"",
            "dir{",
            "architecture\"",
            "architecture{",
            "architectures",
            "#include",
            "#clear",
        ];
        let mut files = vec![root.path("/etc/apt/apt.conf")];
        if let Ok(dir) = std::fs::read_dir(root.path("/etc/apt/apt.conf.d")) {
            files.extend(dir.filter_map(|entry| Some(entry.ok()?.path())));
        }
        for file in files.into_iter().filter(|file| file.is_file()) {
            let text = std::fs::read(&file)
                .map_err(|err| format!("{} cannot be read: {err}", file.display()))?;
            let squeezed: String = String::from_utf8_lossy(&text)
                .chars()
                .filter(|c| !c.is_whitespace())
                .map(|c| c.to_ascii_lowercase())
                .collect();
            if let Some(needle) = NEEDLES.iter().find(|needle| squeezed.contains(**needle)) {
                return Err(format!(
                    "{} sets {needle}, which the native does not follow",
                    file.display()
                ));
            }
        }
        Ok(())
    }
}
