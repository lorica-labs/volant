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

#[cfg(unix)]
mod imp {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use serde_json::{Map, Value};
    use volant_protocol::TaskResult;

    use crate::modules::Context;
    use crate::modules::command::CANCEL_POLL;
    use crate::natives::NativeRun;
    use crate::natives::setup::{Clock, Root, Stop};

    const STATUS: &str = "/var/lib/dpkg/status";
    const LISTS: &str = "/var/lib/apt/lists";

    /// The fields apt's `VersionHash` reads: a list's version and the status's are one record only
    /// when these agree.
    const HASHED: [&str; 6] = [
        "Installed-Size",
        "Depends",
        "Pre-Depends",
        "Conflicts",
        "Breaks",
        "Replaces",
    ];

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
    fn module_env(context: &Context) -> BTreeMap<String, String> {
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
        manager: Vec<String>,
        apt_named: bool,
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
            .map(|mut dir| dir.next().is_some())
            .unwrap_or(false);
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

    fn read_text(root: &Root, path: &str) -> Result<String, String> {
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

    /// One installed package as dpkg's status records it.
    #[derive(Debug)]
    pub(super) struct Installed<'a> {
        version: &'a str,
        arch: &'a str,
        section: Option<&'a str>,
        multi_arch: &'a str,
        hashed: [Option<&'a str>; 6],
    }

    /// The paragraphs of a deb822 file.
    fn stanzas(text: &str) -> impl Iterator<Item = &str> {
        text.split("\n\n")
            .filter(|stanza| !stanza.trim().is_empty())
    }

    /// A field's value, continuation lines included, trimmed; names compare without case.
    fn field<'a>(stanza: &'a str, name: &str) -> Option<&'a str> {
        let mut lines = stanza.split('\n').peekable();
        while let Some(line) = lines.next() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if line.starts_with([' ', '\t']) || !key.eq_ignore_ascii_case(name) {
                continue;
            }
            let start = value.as_ptr() as usize - stanza.as_ptr() as usize;
            let mut end = start + value.len();
            while let Some(next) = lines.peek() {
                if !next.starts_with([' ', '\t']) {
                    break;
                }
                end = next.as_ptr() as usize - stanza.as_ptr() as usize + next.len();
                lines.next();
            }
            return Some(stanza[start..end].trim());
        }
        None
    }

    /// `Multi-Arch` as apt reads it: absent is `no`; a value apt does not know hands back.
    fn multi_arch(stanza: &str) -> Result<&str, String> {
        match field(stanza, "Multi-Arch") {
            None => Ok("no"),
            Some(value @ ("no" | "same" | "foreign" | "allowed")) => Ok(value),
            Some(value) => Err(format!("Multi-Arch: {value} is not one apt knows")),
        }
    }

    fn hashed(stanza: &str) -> [Option<&str>; 6] {
        HASHED.map(|name| field(stanza, name))
    }

    /// The native architecture (dpkg's own) and the packages whose `current_ver` apt sets.
    ///
    /// apt sets it for every state but `not-installed` and `config-files`, so `half-configured`
    /// or `unpacked` would be listed as installed: the native hands those back rather than
    /// reading them.
    pub(super) fn installed(status: &str) -> Result<(&str, HashMap<&str, Installed<'_>>), String> {
        let mut installed = HashMap::new();
        let mut native = None;
        for stanza in stanzas(status) {
            let name = field(stanza, "Package").ok_or("a status paragraph has no Package")?;
            let state = field(stanza, "Status").unwrap_or_default();
            let words: Vec<&str> = state.split_whitespace().collect();
            let [_want, flag, current] = words[..] else {
                return Err(format!("{name} has the status {state:?}"));
            };
            if matches!(current, "not-installed" | "config-files") {
                continue;
            }
            if flag != "ok" || current != "installed" {
                return Err(format!(
                    "{name} is {flag} {current}, which the native does not read as installed"
                ));
            }
            let (Some(version), Some(arch)) =
                (field(stanza, "Version"), field(stanza, "Architecture"))
            else {
                return Err(format!(
                    "{name} is installed without a version or an architecture"
                ));
            };
            if name == "dpkg" {
                native = Some(arch);
            }
            let package = Installed {
                version,
                arch,
                section: field(stanza, "Section"),
                multi_arch: multi_arch(stanza)?,
                hashed: hashed(stanza),
            };
            if installed.insert(name, package).is_some() {
                return Err(format!("{name} is installed twice"));
            }
        }
        let native = native.ok_or("dpkg itself is not installed")?;
        if let Some((name, package)) = installed
            .iter()
            .find(|(_, package)| package.arch != native && package.arch != "all")
        {
            return Err(format!(
                "{name} is installed for the foreign architecture {}",
                package.arch
            ));
        }
        Ok((native, installed))
    }

    /// One configured `deb` source: the prefix of its files in apt's lists, and the names of the
    /// `Packages` files apt keeps for it.
    #[derive(Debug, PartialEq)]
    pub(super) struct Source {
        prefix: String,
        packages: Vec<String>,
    }

    /// Every `deb` source of `sources.list` and `sources.list.d`, one-line and deb822 alike.
    ///
    /// apt refuses the whole list when two entries for one URI and suite, `deb-src` ones
    /// included, set `Signed-By` or `Trusted` differently (apt 2.8 counts an absent one as a
    /// value): the module then fails, and the native hands back.
    pub(super) fn sources(
        root: &Root,
        native: &str,
        foreign: &[String],
    ) -> Result<Vec<Source>, String> {
        let mut default_archs = vec![native.to_string()];
        default_archs.extend(foreign.iter().cloned());
        let mut entries = Vec::new();
        if root.is_file("/etc/apt/sources.list") {
            one_line(&read_text(root, "/etc/apt/sources.list")?, &mut entries)?;
        }
        if let Ok(dir) = std::fs::read_dir(root.path("/etc/apt/sources.list.d")) {
            for entry in dir {
                let entry = entry.map_err(|err| format!("sources.list.d cannot be read: {err}"))?;
                let name = entry.file_name();
                let name = name.to_str().ok_or("a source file's name is not UTF-8")?;
                let one = name.ends_with(".list");
                if !one && !name.ends_with(".sources") {
                    continue;
                }
                if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
                {
                    return Err(format!("the source file {name} has a name apt may skip"));
                }
                let text = read_text(root, &format!("/etc/apt/sources.list.d/{name}"))?;
                if one {
                    one_line(&text, &mut entries)?;
                } else {
                    deb822(&text, &mut entries)?;
                }
            }
        }
        let mut trust: HashMap<String, &[Option<String>; 2]> = HashMap::new();
        let mut sources = Vec::new();
        for entry in &entries {
            let prefix = prefix(&entry.uri, &entry.suite)?;
            if trust
                .insert(prefix.clone(), &entry.trust)
                .is_some_and(|seen| *seen != entry.trust)
            {
                return Err(format!(
                    "{} {} is given two different Signed-By or Trusted",
                    entry.uri, entry.suite
                ));
            }
            if !entry.binary {
                continue;
            }
            let archs = entry.archs.as_ref().unwrap_or(&default_archs);
            let mut packages = Vec::new();
            for component in &entry.components {
                if !component
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
                {
                    return Err(format!("the component {component} is not a plain name"));
                }
                for arch in archs {
                    packages.push(format!("{prefix}{component}_binary-{arch}_Packages"));
                }
            }
            if packages.is_empty() {
                return Err(format!("{} {} names no component", entry.uri, entry.suite));
            }
            sources.push(Source { prefix, packages });
        }
        Ok(sources)
    }

    struct Entry {
        /// `deb`, rather than `deb-src`.
        binary: bool,
        uri: String,
        suite: String,
        components: Vec<String>,
        archs: Option<Vec<String>>,
        /// `Signed-By` and `Trusted`, as written.
        trust: [Option<String>; 2],
    }

    /// Options that change which files apt reads, or where: the native does not follow them.
    const OPTIONS_HANDED_BACK: [&str; 7] = [
        "targets",
        "architectures-add",
        "architectures-remove",
        "inrelease-path",
        "snapshot",
        "include",
        "exclude",
    ];

    /// `sources.list`'s one-line form: `deb [options] uri suite component...`.
    fn one_line(text: &str, entries: &mut Vec<Entry>) -> Result<(), String> {
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default();
            let mut words = line.split_whitespace().peekable();
            let Some(kind) = words.next() else {
                continue;
            };
            let mut options = Vec::new();
            if words.peek().is_some_and(|word| word.starts_with('[')) {
                for word in words.by_ref() {
                    let done = word.ends_with(']');
                    let word = word.trim_start_matches('[').trim_end_matches(']');
                    options.extend(word.split_whitespace().map(str::to_string));
                    if done {
                        break;
                    }
                }
            }
            let words: Vec<&str> = words.collect();
            let binary = match kind {
                "deb" => true,
                "deb-src" => false,
                _ => return Err(format!("the source type {kind} is not deb")),
            };
            let [uri, suite, components @ ..] = &words[..] else {
                return Err(format!("the source line {line:?} is incomplete"));
            };
            let mut archs = None;
            let mut enabled = true;
            let mut trust = [None, None];
            for option in options {
                let (key, value) = option.split_once('=').unwrap_or((&option, ""));
                let key = key.to_ascii_lowercase();
                match key.as_str() {
                    "arch" => archs = Some(value.split(',').map(str::to_string).collect()),
                    "enabled" => enabled = value != "no",
                    "signed-by" => trust[0] = Some(value.to_string()),
                    "trusted" => trust[1] = Some(value.to_string()),
                    "target" | "inrelease-path" | "snapshot" | "include" | "exclude" => {
                        return Err(format!("the source option {option} changes its lists"));
                    }
                    _ if key.ends_with('+') || key.ends_with('-') => {
                        return Err(format!("the source option {option} changes its lists"));
                    }
                    _ => {}
                }
            }
            if enabled {
                entries.push(Entry {
                    binary,
                    uri: (*uri).to_string(),
                    suite: (*suite).to_string(),
                    components: components.iter().map(|c| (*c).to_string()).collect(),
                    archs,
                    trust,
                });
            }
        }
        Ok(())
    }

    /// The deb822 form of `*.sources`: one entry per type, URI and suite of each paragraph.
    fn deb822(text: &str, entries: &mut Vec<Entry>) -> Result<(), String> {
        let mut paragraphs = vec![String::new()];
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let current = paragraphs.last_mut().expect("never empty");
            if line.trim().is_empty() {
                if !current.is_empty() {
                    paragraphs.push(String::new());
                }
            } else {
                current.push_str(line);
                current.push('\n');
            }
        }
        for stanza in paragraphs.iter().filter(|stanza| !stanza.is_empty()) {
            let stanza = stanza.as_str();
            for line in stanza.split('\n') {
                if let Some((key, _)) = line.split_once(':')
                    && !line.starts_with([' ', '\t'])
                    && OPTIONS_HANDED_BACK.contains(&key.trim().to_ascii_lowercase().as_str())
                {
                    return Err(format!("the source field {key} changes its lists"));
                }
            }
            let words = |name: &str| -> Vec<String> {
                field(stanza, name).map_or(Vec::new(), |value| {
                    value.split_whitespace().map(str::to_string).collect()
                })
            };
            if field(stanza, "Enabled").is_some_and(|value| value.eq_ignore_ascii_case("no")) {
                continue;
            }
            let archs = field(stanza, "Architectures").map(|_| words("Architectures"));
            let trust =
                ["Signed-By", "Trusted"].map(|name| field(stanza, name).map(str::to_string));
            for kind in words("Types") {
                let binary = match kind.as_str() {
                    "deb" => true,
                    "deb-src" => false,
                    _ => return Err(format!("the source type {kind} is not deb")),
                };
                for uri in words("URIs") {
                    for suite in words("Suites") {
                        entries.push(Entry {
                            binary,
                            uri: uri.clone(),
                            suite,
                            components: words("Components"),
                            archs: archs.clone(),
                            trust: trust.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// apt's `URItoFileName` of `<uri>/dists/<suite>/`: the URI without its scheme, `/` turned
    /// into `_`. Only plain http(s) URIs and suites, which it leaves otherwise alone.
    pub(super) fn prefix(uri: &str, suite: &str) -> Result<String, String> {
        let rest = uri
            .strip_prefix("http://")
            .or_else(|| uri.strip_prefix("https://"))
            .ok_or_else(|| format!("the source {uri} is not http or https"))?;
        let plain = |text: &str, extra: &str| {
            !text.is_empty()
                && text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
        };
        if !plain(rest, ".-/:") || rest.contains("//") {
            return Err(format!("the source {uri} is not a plain URI"));
        }
        if !plain(suite, ".-/") || suite.ends_with('/') {
            return Err(format!("the suite {suite} is not a plain name"));
        }
        let rest = rest.trim_end_matches('/');
        Ok(format!(
            "{}_dists_{}_",
            rest.replace('/', "_"),
            suite.replace('/', "_")
        ))
    }

    /// The `Origin` of the `InRelease` (else `Release`) beside a source's lists: what python-apt
    /// gives as `origin` for a version found there.
    pub(super) fn origin(root: &Root, prefix: &str) -> Result<String, String> {
        let text = ["InRelease", "Release"]
            .iter()
            .map(|name| format!("{LISTS}/{prefix}{name}"))
            .find(|path| root.is_file(path))
            .ok_or_else(|| format!("{prefix} has lists and no Release"))
            .and_then(|path| read_text(root, &path))?;
        let body = match text.strip_prefix("-----BEGIN PGP SIGNED MESSAGE-----\n") {
            Some(signed) => signed
                .split_once("\n\n")
                .map(|(_, body)| body)
                .ok_or_else(|| format!("{prefix}InRelease has no body"))?,
            None => &text,
        };
        let first = body.split("\n\n").next().unwrap_or_default();
        field(first, "Origin")
            .map(str::to_string)
            .ok_or_else(|| format!("{prefix} has a Release without an Origin"))
    }

    /// One version of an installed package's name found in a list.
    #[derive(Debug)]
    pub(super) struct Listed {
        origin: String,
        version: String,
        arch: String,
        section: Option<String>,
        size: Option<String>,
        multi_arch: String,
        hashed: [Option<String>; 6],
    }

    type ScanResult = Result<Vec<(String, Listed)>, Stop>;

    /// Every list's versions of the installed names, read in parallel. The cancel is only asked
    /// from this thread; on it, or at the deadline, the readers are told to stop.
    fn scan_all(
        files: &[(PathBuf, String)],
        installed: &HashMap<&str, Installed<'_>>,
        clock: Clock,
        scan: impl Fn(&PathBuf, &str, &HashMap<&str, Installed<'_>>, &AtomicBool) -> ScanResult + Sync,
    ) -> Result<HashMap<String, Vec<Listed>>, Stop> {
        let scan = &scan;
        let halt = AtomicBool::new(false);
        let (sent, answer) = mpsc::channel();
        std::thread::scope(|scope| {
            for (path, origin) in files {
                let sent = sent.clone();
                let halt = &halt;
                scope.spawn(move || {
                    let _ = sent.send(scan(path, origin, installed, halt));
                });
            }
            drop(sent);
            let mut found: HashMap<String, Vec<Listed>> = HashMap::new();
            let mut left = files.len();
            while left > 0 {
                match answer.recv_timeout(CANCEL_POLL) {
                    Ok(listed) => {
                        left -= 1;
                        for (name, listed) in listed? {
                            found.entry(name).or_default().push(listed);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if (clock.cancelled)() {
                            halt.store(true, Ordering::Relaxed);
                            return Err(Stop::Cancelled);
                        }
                        if clock
                            .deadline
                            .is_some_and(|deadline| Instant::now() >= deadline)
                        {
                            halt.store(true, Ordering::Relaxed);
                            return Err(Stop::TimedOut);
                        }
                    }
                }
            }
            Ok(found)
        })
    }

    /// One list's versions of the installed names. Stops early, with what it has, when `halt`
    /// is raised: the caller has already given up on the answer.
    fn scan(
        path: &PathBuf,
        origin: &str,
        installed: &HashMap<&str, Installed<'_>>,
        halt: &AtomicBool,
    ) -> ScanResult {
        let bytes = std::fs::read(path)
            .map_err(|err| format!("{} cannot be read: {err}", path.display()))?;
        let text =
            String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
        let mut listed = Vec::new();
        for (count, stanza) in stanzas(&text).enumerate() {
            if count % 1024 == 0 && halt.load(Ordering::Relaxed) {
                break;
            }
            // Every list paragraph starts with its `Package`; reading only that line skips the
            // thousands of packages this host does not have.
            let name = match stanza.trim_start_matches('\n').strip_prefix("Package: ") {
                Some(rest) => rest.split('\n').next().unwrap_or_default().trim(),
                None => field(stanza, "Package").unwrap_or_default(),
            };
            if !installed.contains_key(name) {
                continue;
            }
            let (Some(version), Some(arch)) =
                (field(stanza, "Version"), field(stanza, "Architecture"))
            else {
                return Err(
                    format!("{name} is listed without a version or an architecture").into(),
                );
            };
            listed.push((
                name.to_string(),
                Listed {
                    origin: origin.to_string(),
                    version: version.to_string(),
                    arch: arch.to_string(),
                    section: field(stanza, "Section").map(str::to_string),
                    size: field(stanza, "Size").map(str::to_string),
                    multi_arch: multi_arch(stanza)?.to_string(),
                    hashed: hashed(stanza).map(|value| value.map(str::to_string)),
                },
            ));
        }
        Ok(listed)
    }

    /// The section and origin of the record apt makes the package's current version: the lists'
    /// when a list carries the installed version with the fields apt hashes unchanged, the
    /// status's own (and the empty origin) when none does.
    pub(super) fn record(
        name: &str,
        package: &Installed<'_>,
        listed: &[Listed],
    ) -> Result<(String, String), String> {
        let mut same = Vec::new();
        for list in listed.iter().filter(|list| list.arch == package.arch) {
            if list.version != package.version {
                if canonical(&list.version) == canonical(package.version) {
                    return Err(format!(
                        "{name} is listed as {} and installed as {}, which apt compares equal",
                        list.version, package.version
                    ));
                }
                continue;
            }
            let hashed_same = list
                .hashed
                .iter()
                .zip(package.hashed)
                .all(|(list, status)| list.as_deref() == status);
            if !hashed_same || list.multi_arch != package.multi_arch {
                return Err(format!(
                    "{name} {} is listed with fields apt hashes that differ from the status",
                    package.version
                ));
            }
            same.push(list);
        }
        let Some(first) = same.first() else {
            let section = package
                .section
                .ok_or_else(|| format!("{name} has no section"))?;
            return Ok((section.to_string(), String::new()));
        };
        // apt keeps the first list's record, with the lists in the order of the sources; when
        // they all agree, that order does not matter.
        if same.iter().any(|list| {
            list.origin != first.origin || list.section != first.section || list.size != first.size
        }) {
            return Err(format!(
                "{name} {} is listed in sources that disagree about it",
                package.version
            ));
        }
        let section = first
            .section
            .clone()
            .ok_or_else(|| format!("{name} is listed without a section"))?;
        Ok((section, first.origin.clone()))
    }

    /// A version as dpkg compares it: epoch, upstream and revision with the leading zeros of each
    /// number dropped and an absent part read as zero. Two versions are equal for apt exactly
    /// when these are.
    pub(super) fn canonical(version: &str) -> String {
        let (epoch, rest) = version.split_once(':').unwrap_or(("", version));
        let (upstream, revision) = rest.rsplit_once('-').unwrap_or((rest, ""));
        let numbers = |part: &str| {
            let mut out = String::new();
            let mut digits = String::new();
            for c in part.chars() {
                if c.is_ascii_digit() {
                    digits.push(c);
                } else {
                    out.push_str(digits.trim_start_matches('0'));
                    digits.clear();
                    out.push(c);
                }
            }
            out.push_str(digits.trim_start_matches('0'));
            out
        };
        format!(
            "{}:{}-{}",
            numbers(epoch),
            numbers(upstream),
            numbers(revision)
        )
    }

    #[cfg(test)]
    mod tests {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        use serde_json::json;

        use super::*;
        use crate::natives::setup::unbounded;

        /// A host as files: `=== <path>` starts each file, the lines up to the next header are its
        /// content. `/usr/bin/python3` stands for the interpreter the reference respawns under.
        const HOST: &str = r#"=== /usr/bin/python3
=== /etc/apt/sources.list.d/ubuntu.sources
Types: deb
URIs: http://archive.ubuntu.com/ubuntu/
Suites: noble noble-updates
Components: main universe
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb-src
URIs: http://archive.ubuntu.com/ubuntu/
Suites: noble
Components: main
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
=== /etc/apt/sources.list.d/example.list
# a third-party repository
deb [signed-by=/usr/share/keyrings/example.gpg] https://repo.example.org/debian stable main
=== /etc/apt/sources.list.d/example.list.save
deb https://old.example.org/debian stable main
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble_InRelease
-----BEGIN PGP SIGNED MESSAGE-----
Hash: SHA512

Origin: Ubuntu
Label: Ubuntu
Suite: noble
Version: 24.04
Codename: noble
Architectures: amd64 arm64 armhf i386 ppc64el riscv64 s390x
Components: main restricted universe multiverse
Description: Ubuntu Noble 24.04
-----BEGIN PGP SIGNATURE-----

iQIzBAEBCgAdFiEEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
=AAAA
-----END PGP SIGNATURE-----
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble-updates_Release
Origin: Ubuntu
Label: Ubuntu
Suite: noble-updates
Codename: noble
Components: main restricted universe multiverse
Description: Ubuntu Noble Updates
=== /var/lib/apt/lists/repo.example.org_debian_dists_stable_InRelease
-----BEGIN PGP SIGNED MESSAGE-----
Hash: SHA256

Origin: Example
Label: Example
Suite: stable
Codename: stable
Components: main
-----BEGIN PGP SIGNATURE-----

iQIzBAEBCgAdFiEEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
=AAAA
-----END PGP SIGNATURE-----
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble_main_binary-amd64_Packages
Package: base-files
Architecture: amd64
Version: 13ubuntu10
Multi-Arch: foreign
Priority: required
Essential: yes
Section: admin
Installed-Size: 404
Size: 72952
Description: Debian base system miscellaneous files

Package: bash
Architecture: amd64
Version: 5.2.21-2ubuntu4
Multi-Arch: foreign
Priority: required
Essential: yes
Section: shells
Installed-Size: 1848
Pre-Depends: libc6 (>= 2.36), libtinfo6 (>= 6)
Depends: base-files (>= 2.1.12), debianutils (>= 5.6-0.1)
Replaces: bash-completion (<< 20060301-0), bash-doc (<= 2.05-1)
Size: 796066
Description: GNU Bourne Again SHell

Package: dpkg
Architecture: amd64
Version: 1.22.6ubuntu6
Multi-Arch: foreign
Priority: required
Essential: yes
Section: admin
Installed-Size: 6476
Pre-Depends: libbz2-1.0, libc6 (>= 2.38), liblzma5 (>= 5.4.0), libzstd1 (>= 1.5.5)
Depends: tar (>= 1.28-1)
Size: 1275398
Description: Debian package management system

Package: held
Architecture: amd64
Version: 1.4-1
Section: utils
Installed-Size: 20
Size: 4000
Description: a package on hold

Package: python3-apt
Architecture: amd64
Version: 2.7.7ubuntu1
Priority: important
Section: python
Installed-Size: 736
Depends: python3 (<< 3.13), python3 (>= 3.12~), python3:any, libapt-pkg6.0t64 (>= 2.7.11)
Size: 168406
Description: Python 3 interface to libapt-pkg

Package: tzdata
Architecture: all
Version: 2024a-2ubuntu1
Multi-Arch: foreign
Priority: important
Section: localization
Installed-Size: 1400
Depends: debconf (>= 0.5) | debconf-2.0
Size: 273024
Description: time zone and daylight-saving time data
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble_universe_binary-amd64_Packages
Package: hello
Architecture: amd64
Version: 2.10-3build2
Priority: optional
Section: universe/devel
Installed-Size: 112
Depends: libc6 (>= 2.34)
Size: 30534
Description: example package based on GNU hello

Package: tool
Architecture: amd64
Version: 2.0-1
Section: universe/utils
Installed-Size: 50
Size: 9000
Description: a tool the archive carries in another version
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble-updates_main_binary-amd64_Packages
Package: base-files
Architecture: amd64
Version: 13ubuntu10.2
Multi-Arch: foreign
Priority: required
Essential: yes
Section: admin
Installed-Size: 405
Size: 73000
Description: Debian base system miscellaneous files

Package: bash
Architecture: amd64
Version: 5.2.21-2ubuntu4
Multi-Arch: foreign
Priority: required
Essential: yes
Section: shells
Installed-Size: 1848
Pre-Depends: libc6 (>= 2.36), libtinfo6 (>= 6)
Depends: base-files (>= 2.1.12), debianutils (>= 5.6-0.1)
Replaces: bash-completion (<< 20060301-0), bash-doc (<= 2.05-1)
Size: 796066
Description: GNU Bourne Again SHell
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble-updates_universe_binary-amd64_Packages
=== /var/lib/apt/lists/repo.example.org_debian_dists_stable_main_binary-amd64_Packages
Package: tool
Version: 1.0-1
Architecture: amd64
Section: utils
Installed-Size: 48
Size: 8800
Description: a tool from a third-party repository
=== /var/lib/dpkg/status
Package: base-files
Essential: yes
Status: install ok installed
Priority: required
Section: admin
Installed-Size: 404
Maintainer: Ubuntu Developers <ubuntu-devel-discuss@lists.ubuntu.com>
Architecture: amd64
Multi-Arch: foreign
Version: 13ubuntu10.1
Description: Debian base system miscellaneous files

Package: bash
Essential: yes
Status: install ok installed
Priority: required
Section: shells
Installed-Size: 1848
Architecture: amd64
Multi-Arch: foreign
Version: 5.2.21-2ubuntu4
Replaces: bash-completion (<< 20060301-0), bash-doc (<= 2.05-1)
Depends: base-files (>= 2.1.12), debianutils (>= 5.6-0.1)
Pre-Depends: libc6 (>= 2.36), libtinfo6 (>= 6)
Conffiles:
 /etc/bash.bashrc 89269e1298235f1b12b4c16e4065ad0d
 /etc/skel/.bashrc 0f1a3c5b0ad6d1e3a4bc86bb9d71ba6b
Description: GNU Bourne Again SHell

Package: dpkg
Essential: yes
Status: install ok installed
Priority: required
Section: admin
Installed-Size: 6476
Architecture: amd64
Multi-Arch: foreign
Version: 1.22.6ubuntu6
Depends: tar (>= 1.28-1)
Pre-Depends: libbz2-1.0, libc6 (>= 2.38), liblzma5 (>= 5.4.0), libzstd1 (>= 1.5.5)
Description: Debian package management system

Package: held
Status: hold ok installed
Section: utils
Installed-Size: 20
Architecture: amd64
Version: 1.4-1
Description: a package on hold

Package: hello
Status: install ok installed
Priority: optional
Section: devel
Installed-Size: 112
Architecture: amd64
Version: 2.10-3build2
Depends: libc6 (>= 2.34)
Description: example package based on GNU hello

Package: oldpkg
Status: deinstall ok config-files
Priority: optional
Section: misc
Installed-Size: 10
Architecture: amd64
Version: 0.9-1
Conffiles:
 /etc/oldpkg.conf 00000000000000000000000000000000
Description: a removed package whose configuration stays

Package: python3-apt
Status: install ok installed
Priority: important
Section: python
Installed-Size: 736
Architecture: amd64
Version: 2.7.7ubuntu1
Depends: python3 (<< 3.13), python3 (>= 3.12~), python3:any, libapt-pkg6.0t64 (>= 2.7.11)
Description: Python 3 interface to libapt-pkg

Package: tool
Status: install ok installed
Section: utils
Installed-Size: 48
Architecture: amd64
Version: 1.0-1
Description: a tool from a third-party repository

Package: tzdata
Status: install ok installed
Priority: important
Section: localization
Installed-Size: 1400
Architecture: all
Multi-Arch: foreign
Version: 2024a-2ubuntu1
Depends: debconf (>= 0.5) | debconf-2.0
Description: time zone and daylight-saving time data
"#;

        /// What python-apt 2.7 and 3.1 (`apt.Cache(rootdir=...)`, read the way `package_facts`
        /// reads it) say of `HOST`. Measured on both: `hello`'s section is the archive's, not
        /// the status's; `tool` takes the origin of the list that carries its installed
        /// version, not the one that carries its name; `base-files` is in no list at its
        /// installed version and gets the empty origin; `held` (on hold) is installed;
        /// `oldpkg` (configuration only) is not.
        const REFERENCE: &str = r#"{
    "base-files": [{"arch": "amd64", "category": "admin", "name": "base-files", "origin": "", "source": "apt", "version": "13ubuntu10.1"}],
    "bash": [{"arch": "amd64", "category": "shells", "name": "bash", "origin": "Ubuntu", "source": "apt", "version": "5.2.21-2ubuntu4"}],
    "dpkg": [{"arch": "amd64", "category": "admin", "name": "dpkg", "origin": "Ubuntu", "source": "apt", "version": "1.22.6ubuntu6"}],
    "held": [{"arch": "amd64", "category": "utils", "name": "held", "origin": "Ubuntu", "source": "apt", "version": "1.4-1"}],
    "hello": [{"arch": "amd64", "category": "universe/devel", "name": "hello", "origin": "Ubuntu", "source": "apt", "version": "2.10-3build2"}],
    "python3-apt": [{"arch": "amd64", "category": "python", "name": "python3-apt", "origin": "Ubuntu", "source": "apt", "version": "2.7.7ubuntu1"}],
    "tool": [{"arch": "amd64", "category": "utils", "name": "tool", "origin": "Example", "source": "apt", "version": "1.0-1"}],
    "tzdata": [{"arch": "all", "category": "localization", "name": "tzdata", "origin": "Ubuntu", "source": "apt", "version": "2024a-2ubuntu1"}]
}"#;

        /// `HOST` written under a directory of its own, removed at the end.
        struct Tree(PathBuf);

        impl Drop for Tree {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        impl Tree {
            fn new(host: &str) -> Tree {
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                let dir = PathBuf::from(format!(
                    "/tmp/volant-package-facts-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(dir.join("var/lib/apt/lists/partial")).unwrap();
                std::fs::create_dir_all(dir.join("var/lib/dpkg/updates")).unwrap();
                let tree = Tree(dir);
                let mut path: Option<String> = None;
                let mut content = String::new();
                for line in host.split_inclusive('\n').chain(std::iter::once("=== \n")) {
                    let Some(next) = line.strip_prefix("=== ") else {
                        content.push_str(line);
                        continue;
                    };
                    if let Some(path) = path.take() {
                        tree.write(&path, &content);
                    }
                    content.clear();
                    path = Some(next.trim().to_string()).filter(|next| !next.is_empty());
                }
                tree
            }

            fn write(&self, path: &str, content: &str) {
                let full = self.0.join(path.trim_start_matches('/'));
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(&full, content).unwrap();
                if path.starts_with("/usr/bin/") {
                    std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                }
            }

            fn edit(&self, path: &str, from: &str, to: &str) {
                let full = self.0.join(path.trim_start_matches('/'));
                let text = std::fs::read_to_string(&full).unwrap();
                assert!(text.contains(from), "{path} has no {from:?}");
                std::fs::write(&full, text.replacen(from, to, 1)).unwrap();
            }

            fn answer(&self, args: Value) -> Result<Map<String, Value>, Stop> {
                self.answer_with(args, &[])
            }

            fn answer_with(
                &self,
                args: Value,
                env: &[(&str, &str)],
            ) -> Result<Map<String, Value>, Stop> {
                let mut module_env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".into())]);
                module_env.extend(
                    env.iter()
                        .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
                );
                answer(
                    args.as_object().unwrap(),
                    &Root::at(&self.0),
                    &module_env,
                    unbounded(),
                )
            }

            fn hands_back(&self, args: Value) -> String {
                match self.answer(args) {
                    Err(Stop::HandBack(reason)) => reason,
                    other => panic!("answered where the native should hand back: {other:?}"),
                }
            }
        }

        /// The fixture host, answered as python-apt answers it, with the invocation the
        /// reference prints for the default arguments.
        ///
        /// What would make this red: the origin taken from a list that carries the package's
        /// name at another version (`tool` would say `Ubuntu`); the section taken from the
        /// status when a list carries the version (`hello` would say `devel`); a version no
        /// list carries given a list's origin (`base-files`); `hold` read as not installed, or
        /// `config-files` read as installed.
        #[test]
        fn a_host_is_answered_as_python_apt_answers_it() {
            let tree = Tree::new(HOST);
            let result = tree.answer(json!({})).unwrap();
            let reference: Value = serde_json::from_str(REFERENCE).unwrap();
            assert_eq!(result["ansible_facts"]["packages"], reference);
            assert_eq!(
                result["invocation"],
                json!({"module_args": {"manager": ["auto"], "strategy": "first"}})
            );
            let result = tree
                .answer(json!({"manager": "APT", "strategy": "first"}))
                .unwrap();
            assert_eq!(result["ansible_facts"]["packages"], reference);
            assert_eq!(
                result["invocation"],
                json!({"module_args": {"manager": ["APT"], "strategy": "first"}}),
                "the invocation keeps the manager as written, converted to a list"
            );
        }

        /// Every state apt counts as installed besides `installed` itself hands back: a
        /// half-configured package is listed by the reference, and the native does not read it.
        ///
        /// What would make this red: `half-configured` (or `unpacked`, `triggers-pending`, a
        /// `reinstreq` flag) read as `installed`, or skipped as not installed.
        #[test]
        fn a_half_configured_package_hands_back() {
            for state in [
                "install ok half-configured",
                "install ok unpacked",
                "install ok half-installed",
                "install ok triggers-pending",
                "install reinstreq installed",
            ] {
                let tree = Tree::new(HOST);
                tree.edit(
                    "/var/lib/dpkg/status",
                    "Package: hello\nStatus: install ok installed",
                    &format!("Package: hello\nStatus: {state}"),
                );
                let reason = tree.hands_back(json!({}));
                assert!(
                    reason.starts_with("hello is ") && reason.ends_with("as installed"),
                    "{state}: {reason}"
                );
            }
        }

        /// Hosts the native does not read hand back before answering.
        ///
        /// What would make this red, one line each: a list no source names read (a stale list
        /// apt ignores); a compressed list skipped; `apk` ignored under `auto`; `APT_CONFIG`
        /// or `Dir::State` ignored; dpkg's pending updates ignored; a foreign architecture
        /// answered; a source whose `deb-src` twin sets another `Signed-By` answered (apt
        /// refuses the list); python3-apt missing; a list whose version differs from the
        /// status's in a hashed field, or in text only, or two lists disagreeing about one
        /// version, answered.
        #[test]
        fn a_host_outside_the_subset_hands_back() {
            let cases: Vec<(&str, Box<dyn Fn(&Tree)>)> = vec![
                (
                    "is a list no configured source names",
                    Box::new(|tree| {
                        tree.write(
                            &format!("{LISTS}/old.example.org_debian_dists_stable_main_binary-amd64_Packages"),
                            "",
                        );
                    }),
                ),
                (
                    "is a compressed list",
                    Box::new(|tree| {
                        tree.write(
                            &format!("{LISTS}/repo.example.org_debian_dists_stable_main_binary-amd64_Packages.lz4"),
                            "",
                        );
                    }),
                ),
                (
                    "apk is found",
                    Box::new(|tree| tree.write("/usr/bin/apk", "")),
                ),
                (
                    "sets dir::state",
                    Box::new(|tree| {
                        tree.write(
                            "/etc/apt/apt.conf.d/99lists",
                            "Dir::State::Lists \"/srv/lists\";\n",
                        );
                    }),
                ),
                (
                    "sets architecture\"",
                    Box::new(|tree| {
                        tree.write(
                            "/etc/apt/apt.conf.d/99arch",
                            "APT {\n  Architecture \"i386\";\n};\n",
                        );
                    }),
                ),
                (
                    "for the foreign architecture i386",
                    Box::new(|tree| {
                        tree.edit(
                            "/var/lib/dpkg/status",
                            "Architecture: amd64\nVersion: 1.4-1",
                            "Architecture: i386\nVersion: 1.4-1",
                        );
                    }),
                ),
                (
                    "two different Signed-By or Trusted",
                    Box::new(|tree| {
                        tree.edit(
                            "/etc/apt/sources.list.d/ubuntu.sources",
                            "Components: main\nSigned-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg",
                            "Components: main",
                        );
                    }),
                ),
                (
                    "python3-apt is not installed",
                    Box::new(|tree| {
                        tree.edit(
                            "/var/lib/dpkg/status",
                            "Package: python3-apt\nStatus: install ok installed",
                            "Package: python3-apt\nStatus: deinstall ok config-files",
                        );
                    }),
                ),
                (
                    "with fields apt hashes that differ from the status",
                    Box::new(|tree| {
                        tree.edit(
                            "/var/lib/dpkg/status",
                            "Depends: libc6 (>= 2.34)",
                            "Depends: libc6 (>= 2.35)",
                        );
                    }),
                ),
                (
                    "which apt compares equal",
                    Box::new(|tree| {
                        tree.edit(
                            &format!("{LISTS}/repo.example.org_debian_dists_stable_main_binary-amd64_Packages"),
                            "Version: 1.0-1",
                            "Version: 0:1.00-1",
                        );
                    }),
                ),
                (
                    "in sources that disagree about it",
                    Box::new(|tree| {
                        tree.edit(
                            &format!("{LISTS}/archive.ubuntu.com_ubuntu_dists_noble_universe_binary-amd64_Packages"),
                            "Version: 2.0-1\nSection: universe/utils\nInstalled-Size: 50\nSize: 9000",
                            "Version: 1.0-1\nSection: utils\nInstalled-Size: 48\nSize: 8800",
                        );
                    }),
                ),
            ];
            for (reason, break_host) in cases {
                let tree = Tree::new(HOST);
                tree.answer(json!({}))
                    .expect("the fixture host is answered");
                break_host(&tree);
                let given = tree.hands_back(json!({}));
                assert!(given.contains(reason), "expected {reason:?}, got {given:?}");
            }
            let tree = Tree::new(HOST);
            tree.write("/var/lib/dpkg/updates/0001", "");
            assert!(tree.hands_back(json!({})).contains("dpkg has updates"));
            let tree = Tree::new(HOST);
            let given = match tree.answer_with(json!({}), &[("APT_CONFIG", "/srv/apt.conf")]) {
                Err(Stop::HandBack(reason)) => reason,
                other => panic!("APT_CONFIG answered: {other:?}"),
            };
            assert!(given.contains("APT_CONFIG"), "{given}");
        }

        /// `apt` named ahead of `auto` is tried first, so a host with `apk` is still answered.
        #[test]
        fn apt_named_is_answered_before_apk() {
            let tree = Tree::new(HOST);
            tree.write("/usr/bin/apk", "");
            assert!(tree.answer(json!({"manager": ["auto", "apt"]})).is_ok());
        }

        /// The arguments the module validates, converted as `AnsibleModule` converts them, and
        /// everything outside `apt` first handed back for the module to answer in its own words.
        ///
        /// What would make this red: a comma-separated string not split; a second `auto`
        /// accepted (the module refuses it); `strategy: all`, `null`, or another manager
        /// answered; an unknown argument ignored.
        #[test]
        fn the_arguments_are_read_as_the_module_reads_them() {
            let ok = |args: Value| request(args.as_object().unwrap());
            assert_eq!(
                ok(json!({"manager": "auto,apt"})).unwrap(),
                Request {
                    manager: vec!["auto".into(), "apt".into()],
                    apt_named: true
                }
            );
            assert_eq!(
                ok(json!({})).unwrap(),
                Request {
                    manager: vec!["auto".into()],
                    apt_named: false
                }
            );
            for args in [
                json!({"manager": ["auto", "auto"]}),
                json!({"manager": "auto, apt"}),
                json!({"manager": ["rpm"]}),
                json!({"manager": []}),
                json!({"manager": 5}),
                json!({"manager": [5]}),
                json!({"manager": null}),
                json!({"strategy": "all"}),
                json!({"strategy": null}),
                json!({"strategy": "First"}),
                json!({"other": 1}),
            ] {
                assert!(ok(args.clone()).is_err(), "{args} was accepted");
            }
        }

        /// Versions apt compares equal although written differently.
        #[test]
        fn versions_compare_as_dpkg_compares_them() {
            for (left, right) in [
                ("1.0-1", "0:1.0-1"),
                ("1.00-1", "1.0-1"),
                ("1.0", "1.0-0"),
                ("1.0", "1."),
                ("2:007", "2:7"),
            ] {
                assert_eq!(canonical(left), canonical(right), "{left} {right}");
            }
            for (left, right) in [
                ("1.0", "1.0.0"),
                ("1.0-1", "1.0-1ubuntu1"),
                ("1:1.0", "1.0"),
                ("1.0~rc1", "1.0"),
            ] {
                assert_ne!(canonical(left), canonical(right), "{left} {right}");
            }
        }

        /// The lists are read while the task's cancel and deadline are watched: a cancel gives
        /// `Cancelled`, a deadline passed gives `TimedOut`, and the readers are told to stop.
        ///
        /// What would make this red: the wait not polling the cancel or the deadline (the test
        /// then waits on a reader that only stops when told to), or `halt` not raised.
        #[test]
        fn a_cancel_or_the_deadline_stops_the_readers() {
            let files = vec![(PathBuf::from("/nonexistent"), String::new())];
            let installed = HashMap::new();
            let slow =
                |_: &PathBuf, _: &str, _: &HashMap<&str, Installed<'_>>, halt: &AtomicBool| {
                    while !halt.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(Vec::new())
                };
            let yes = || true;
            let clock = Clock {
                deadline: None,
                cancelled: &yes,
            };
            assert!(matches!(
                scan_all(&files, &installed, clock, slow),
                Err(Stop::Cancelled)
            ));
            let no = || false;
            let clock = Clock {
                deadline: Some(Instant::now() + Duration::from_millis(100)),
                cancelled: &no,
            };
            assert!(matches!(
                scan_all(&files, &installed, clock, slow),
                Err(Stop::TimedOut)
            ));
        }

        /// A reader told to stop returns at its next check rather than reading on.
        #[test]
        fn a_reader_told_to_stop_reads_no_further() {
            let tree = Tree::new(HOST);
            let status = std::fs::read_to_string(tree.0.join("var/lib/dpkg/status")).unwrap();
            let (_, installed) = installed(&status).unwrap();
            let path = tree
                .0
                .join("var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble_main_binary-amd64_Packages");
            let halt = AtomicBool::new(true);
            assert!(scan(&path, "Ubuntu", &installed, &halt).unwrap().is_empty());
            halt.store(false, Ordering::Relaxed);
            assert_eq!(scan(&path, "Ubuntu", &installed, &halt).unwrap().len(), 6);
        }
    }
}
