// SPDX-License-Identifier: GPL-3.0-or-later
//! Which gathered facts a run can read, found before the first connection, and whether the
//! agent's native `setup` produces every one of them.
//!
//! The native collector leaves out every key it cannot produce
//! ([`volant_protocol::facts::NATIVE_FACT_KEYS`]), and a missing key shows only once something
//! reads it. So `--facts auto` sends `setup` to the native only when every fact name the run can
//! read is in that list. The scan reads the text as written and never renders it. Whatever it
//! cannot pin to a literal name reads as every fact ([`FactsRead::All`]), which keeps the Python
//! `setup`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use volant_protocol::facts::NATIVE_FACT_KEYS;
use volant_protocol::modules::short_name;

use crate::compile::{Compiled, Origin};
use crate::executor::{expressions, strings, task_strings};
use crate::playbook::{Play, PlayTask};
use crate::python::{Facts, Reach};
use crate::template::Templar;
use crate::vars::VarStore;

/// What a run can read of the facts `setup` gathers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactsRead {
    /// Any fact at all: the construct that can reach one by a computed name, and where it is.
    All(String),
    /// These keys of `ansible_facts`, without the `ansible_` prefix.
    Keys(BTreeSet<String>),
}

/// `ansible_<name>` variables that are not facts: the magic variables and the connection
/// variables of ansible-core 2.19.12. Reading one reads no fact.
const NOT_FACTS: &[&str] = &[
    "async_dir",
    "check_mode",
    "collection_name",
    "config_file",
    "connection",
    "dependent_role_names",
    "diff_mode",
    "facts_parallel",
    "failed_result",
    "failed_task",
    "forks",
    "host",
    "index_var",
    "inventory_sources",
    "limit",
    "loop",
    "loop_var",
    "managed",
    "module_compression",
    "parent_role_names",
    "parent_role_paths",
    "password",
    "pipelining",
    "play_batch",
    "play_hosts",
    "play_hosts_all",
    "play_name",
    "play_role_names",
    "playbook_python",
    "port",
    "private_key_file",
    "remote_tmp",
    "role_name",
    "role_names",
    "run_tags",
    "search_path",
    "shell_executable",
    "shell_type",
    "skip_tags",
    "timeout",
    "user",
    "verbosity",
    "version",
];

/// Prefixes of connection variables. `ssh_` is one too, apart from the host key facts.
const NOT_FACT_PREFIXES: &[&str] = &[
    "become",
    "httpapi_",
    "libssh_",
    "netconf_",
    "network_",
    "paramiko_",
    "persistent_",
    "psrp_",
    "winrm_",
];

/// Keys of `ansible_facts` that other modules write and `setup` never does: reading one reads the
/// same thing whichever `setup` ran.
const OTHER_MODULES_KEYS: &[&str] = &["aggregate_mounts", "mount_points", "packages", "services"];

/// The lookups whose argument names a variable or a template file this scan does not open.
const DYNAMIC_LOOKUPS: &[&str] = &["vars", "template"];

/// Facts whose value is one name and never a path: a file name built from one stays a name in the
/// directory it is looked for in (`include_vars: "{{ ansible_facts.os_family }}.yml"`).
const NAME_FACTS: &[&str] = &[
    "architecture",
    "distribution",
    "distribution_major_version",
    "distribution_release",
    "distribution_version",
    "machine",
    "os_family",
    "pkg_mgr",
    "service_mgr",
    "system",
];

/// The most file names one templated name may stand for before the scan stops counting them.
const MOST_NAMES: usize = 256;

/// In a file name pattern, any run of characters but `/`: what a [`NAME_FACTS`] value renders to.
const ANY: char = '\0';

/// How many times the templated names are resolved again because a file one of them named
/// defined a variable another reads.
const MOST_PASSES: usize = 8;

/// Every fact name the run's plays can read: in the text of every task and handler they reach,
/// every template and vars file in the roles they reach, the body of a template with a literal
/// `src`, a literal `include_vars` file and `vars_files` entry, role parameters, play `vars:`,
/// the inventory and its `group_vars` and `host_vars`, and the command line's variables.
///
/// A role is read whole, used or not: every file under its `templates/`, `vars/`, `defaults/`
/// and `meta/`, and every task file. A templated `src`, `include_vars` or `include_tasks` name
/// counts as read only when every file it can render to is known and read: each variable in it
/// given plain string values alone wherever the scan looks, each loop item from a literal list,
/// each fact one whose value is a single name ([`NAME_FACTS`]), and for `include_tasks` every
/// matching file one the walk read. Anything else reads every fact.
pub(crate) fn facts_read(
    plays: &[(&Play, &Compiled)],
    reach: &Reach,
    store: &VarStore,
) -> FactsRead {
    let mut scan = Scan {
        base: store.playbook_dir().to_path_buf(),
        keys: BTreeSet::new(),
        all: None,
        defs: BTreeMap::new(),
        defs_unknown: false,
        pending: Vec::new(),
        task_files: reach.files.clone(),
    };
    for map in store.static_maps() {
        scan.map(map, "the inventory and its variables");
    }
    let mut roles = reach.roles.clone();
    for (play, compiled) in plays {
        scan.map(&play.vars, "play vars");
        for entry in &play.vars_files {
            if Templar::is_template(entry) {
                scan.dynamic(&format!("vars_files entry {entry}"), "the play");
            } else {
                scan.file(&play.dir.join(entry));
            }
        }
        // The `group_vars` and `host_vars` beside a play from another directory are read when the
        // run gets there; the store holds only the first playbook's.
        if play.dir != scan.base {
            for sub in ["group_vars", "host_vars"] {
                scan.tree(&play.dir.join(sub));
            }
        }
        for role in compiled.roles.iter().chain([&compiled.exported]) {
            for map in [&role.defaults, &role.vars, &role.params] {
                scan.map(map, &format!("the variables of role {}", role.name));
            }
        }
        for step in &compiled.steps {
            scan.task(
                &step.task,
                &step.origin,
                &format!("task '{}'", step.task.name),
            );
        }
        for handler in &compiled.handlers {
            let role_dir = handler
                .role
                .and_then(|i| compiled.roles.get(i))
                .and_then(|role| compiled.search.locate(&role.name).ok());
            let origin = Origin {
                file_dir: role_dir
                    .as_ref()
                    .map_or_else(|| play.dir.clone(), |dir| dir.join("handlers")),
                role_dir: role_dir.clone(),
                ..Origin::default()
            };
            roles.extend(role_dir);
            scan.task(
                &handler.task,
                &origin,
                &format!("handler '{}'", handler.task.name),
            );
        }
    }
    for (file, origin, task) in &reach.tasks {
        let place = scan.shown(file);
        scan.task(task, origin, &place);
    }
    for role in &roles {
        for sub in ["defaults", "vars", "templates", "meta"] {
            scan.tree(&role.join(sub));
        }
    }
    scan.settle();
    match scan.all {
        Some(why) => FactsRead::All(why),
        None => FactsRead::Keys(scan.keys),
    }
}

/// Whether every key `read` names is one the native `setup` produces.
pub(crate) fn native_gather_allowed(read: &FactsRead) -> bool {
    match read {
        FactsRead::All(_) => false,
        FactsRead::Keys(keys) => keys.iter().all(|key| native(key)),
    }
}

fn native(key: &str) -> bool {
    NATIVE_FACT_KEYS
        .iter()
        .any(|native| native.strip_prefix("ansible_") == Some(key))
}

/// Where `setup` gets its facts under `--facts auto`, and the line `--profile` prints for it.
pub(crate) fn decide(read: &FactsRead) -> (Facts, String) {
    match read {
        FactsRead::Keys(keys) if native_gather_allowed(read) => {
            (Facts::Native, format!("native (keys: {})", keys.len()))
        }
        FactsRead::Keys(keys) => {
            let outside: Vec<&str> = keys
                .iter()
                .map(String::as_str)
                .filter(|key| !native(key))
                .collect();
            (
                Facts::Python,
                format!(
                    "python ({} read, not collected natively)",
                    outside.join(", ")
                ),
            )
        }
        FactsRead::All(why) => (Facts::Python, format!("python ({why})")),
    }
}

struct Scan {
    base: PathBuf,
    keys: BTreeSet<String>,
    /// The first construct found that can read any fact, with where it was found.
    all: Option<String>,
    /// Every value each variable is given anywhere the scan reads.
    defs: BTreeMap<String, Vec<Value>>,
    /// Whether some variable is named by a template (`set_fact` with a templated key), so that
    /// no variable's values are all known.
    defs_unknown: bool,
    /// The files named by a template, resolved once every value is known.
    pending: Vec<Pending>,
    /// Every task file the walk read.
    task_files: BTreeSet<PathBuf>,
}

/// What a templated file name is looked for as.
#[derive(Clone, Copy)]
enum Wanted {
    Template,
    Vars,
    Tasks,
}

/// A task whose file name is a template: `template` `src`, `include_vars`, `include_tasks`.
struct Pending {
    wanted: Wanted,
    name: String,
    task: PlayTask,
    origin: Origin,
    place: String,
}

impl Scan {
    fn dynamic(&mut self, what: &str, place: &str) {
        if self.all.is_none() {
            self.all = Some(format!("{what} in {place}"));
        }
    }

    fn key(&mut self, key: &str) {
        let key = key.split('.').next().unwrap_or(key);
        if !OTHER_MODULES_KEYS.contains(&key) {
            self.keys.insert(key.to_string());
        }
    }

    /// `path` relative to the playbook's directory when it is under it.
    fn shown(&self, path: &Path) -> String {
        path.strip_prefix(&self.base)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    fn map(&mut self, map: &Map<String, Value>, place: &str) {
        for (name, value) in map {
            self.define(name, value);
        }
        let mut texts = Vec::new();
        for value in map.values() {
            strings(value, &mut texts);
        }
        for text in texts {
            self.text(text, place);
        }
    }

    /// A file's text, read as a template: a vars file's values and a template body alike. A file
    /// that is not there or not text reads nothing. A file that is a YAML mapping defines its
    /// keys, and under `meta/` every key at any depth (a dependency's parameters).
    fn file(&mut self, path: &Path) {
        if let Ok(text) = std::fs::read_to_string(path) {
            let place = self.shown(path);
            self.text(&text, &place);
        }
        if let Ok(map) = crate::vars::load_vars_file(path) {
            let value = Value::Object(map);
            if path.components().any(|c| c.as_os_str() == "meta") {
                self.define_all(&value);
            } else if let Value::Object(map) = &value {
                for (name, value) in map {
                    self.define(name, value);
                }
            }
        }
    }

    /// `name` is given `value` somewhere.
    fn define(&mut self, name: &str, value: &Value) {
        if Templar::is_template(name) {
            self.defs_unknown = true;
            return;
        }
        let values = self.defs.entry(name.to_string()).or_default();
        if !values.contains(value) {
            values.push(value.clone());
        }
    }

    fn define_all(&mut self, value: &Value) {
        match value {
            Value::Object(map) => {
                for (name, value) in map {
                    self.define(name, value);
                    self.define_all(value);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| self.define_all(v)),
            _ => {}
        }
    }

    /// The first file `name` is found as under `sub`, read; false when there is none.
    fn find(&mut self, origin: &Origin, sub: &str, name: &str) -> bool {
        let found = crate::action_plugins::files::search_paths(origin, &self.base, sub, name)
            .into_iter()
            .find(|p| p.is_file());
        found.map(|path| self.file(&path)).is_some()
    }

    fn later(&mut self, wanted: Wanted, name: &str, task: &PlayTask, origin: &Origin, place: &str) {
        self.pending.push(Pending {
            wanted,
            name: name.to_string(),
            task: task.clone(),
            origin: origin.clone(),
            place: place.to_string(),
        });
    }

    /// Resolves every templated file name. A file one of them reads can define a variable
    /// another one's name uses, so they are resolved until no pass defines anything new, and only
    /// then is a name that cannot be pinned down counted as reading every fact.
    fn settle(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for _ in 0..MOST_PASSES {
            let before = self.defs.clone();
            for p in &pending {
                let _ = self.resolve(p);
            }
            if self.defs == before {
                for p in &pending {
                    if let Err(why) = self.resolve(p) {
                        self.dynamic(&why, &p.place);
                    }
                }
                return;
            }
        }
        self.dynamic("variables defined by the files they name", "the run");
    }

    /// Reads every file `p` can name; an error names what could not be pinned down.
    fn resolve(&mut self, p: &Pending) -> Result<(), String> {
        let (label, sub) = match p.wanted {
            Wanted::Template => ("template", "templates"),
            Wanted::Vars => ("include_vars", "vars"),
            Wanted::Tasks => ("include_tasks", "tasks"),
        };
        let unknown = || format!("{label} {}", p.name);
        if p.task.loop_with.as_deref().map(short_name) == Some("first_found") {
            // `include_tasks: "{{ item }}"` over `with_first_found`: whichever file is found
            // first, in a role's `files/`, the role, its `tasks/` or the playbook's directory.
            let bare = bare_variable(&p.name) == Some(p.task.loop_var.as_str());
            if !matches!(p.wanted, Wanted::Tasks) || !bare {
                return Err(unknown());
            }
            let terms = self.first_found_terms(&p.task).ok_or_else(unknown)?;
            let mut bases: Vec<PathBuf> = Vec::new();
            if let Some(role) = &p.origin.role_dir {
                bases.extend([role.clone(), role.join("tasks")]);
            }
            bases.extend([p.origin.file_dir.clone(), self.base.clone()]);
            let mut candidates = Vec::new();
            for term in &terms {
                for base in &bases {
                    candidates.extend([base.join("files").join(term), base.join(term)]);
                }
            }
            return self.all_read(&candidates, label);
        }
        let names = self.names(&p.name, &p.task, 0).ok_or_else(unknown)?;
        for name in names {
            match p.wanted {
                Wanted::Tasks => {
                    let mut candidates = vec![p.origin.file_dir.join(&name)];
                    candidates.extend(
                        p.origin
                            .role_dir
                            .iter()
                            .map(|role| role.join("tasks").join(&name)),
                    );
                    self.all_read(&candidates, label)?;
                }
                _ if !name.contains(ANY) => {
                    if !self.find(&p.origin, sub, &name) {
                        return Err(format!("{label} {name}, not found"));
                    }
                }
                _ => {
                    let paths = crate::action_plugins::files::search_paths(
                        &p.origin, &self.base, sub, &name,
                    );
                    for found in paths.iter().flat_map(|path| glob(path)) {
                        self.file(&found);
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether every task file matching one of `candidates` is one the walk read.
    fn all_read(&self, candidates: &[PathBuf], label: &str) -> Result<(), String> {
        for candidate in candidates {
            if let Some(unread) = glob(candidate)
                .into_iter()
                .find(|f| !self.task_files.contains(f))
            {
                return Err(format!("{label} {}, a file not read", self.shown(&unread)));
            }
        }
        Ok(())
    }

    /// The file name terms of a `with_first_found` loop, as patterns.
    fn first_found_terms(&self, task: &PlayTask) -> Option<Vec<String>> {
        let Some(Value::Array(terms)) = &task.loop_items else {
            return None;
        };
        let mut out = Vec::new();
        for term in terms {
            out.extend(self.names(term.as_str()?, task, 1)?);
        }
        Some(out)
    }

    /// Every file name `name` can render to, a [`NAME_FACTS`] value standing as [`ANY`]; `None`
    /// when a part of it is not known: a variable given a templated or non-string value
    /// anywhere, or given none the scan sees, a loop item not from a literal list, any other
    /// expression. A loop proven empty names nothing: the task never runs.
    fn names(&self, name: &str, task: &PlayTask, depth: usize) -> Option<Vec<String>> {
        if depth > 2 {
            return None;
        }
        if depth == 0 && self.loop_is_empty(task) {
            return Some(Vec::new());
        }
        let plain = |s: &str| !s.contains("{%") && !s.contains("{#") && !s.contains(ANY);
        let mut out = vec![String::new()];
        let mut rest = name;
        while let Some(at) = rest.find("{{") {
            let (text, after) = (&rest[..at], &rest[at + 2..]);
            let end = after.find("}}")?;
            if !plain(text) {
                return None;
            }
            let values = self.segment(after[..end].trim(), task, depth)?;
            let mut next = Vec::new();
            for prefix in &out {
                for value in &values {
                    next.push(format!("{prefix}{text}{value}"));
                }
            }
            if next.len() > MOST_NAMES {
                return None;
            }
            out = next;
            rest = &after[end + 2..];
        }
        if !plain(rest) {
            return None;
        }
        Some(out.into_iter().map(|prefix| prefix + rest).collect())
    }

    /// What one `{{ }}` of a file name renders to.
    fn segment(&self, e: &str, task: &PlayTask, depth: usize) -> Option<Vec<String>> {
        if e == task.loop_var {
            return self.items(task, depth);
        }
        let fact = e
            .strip_prefix("ansible_facts.")
            .filter(|k| k.bytes().all(is_word))
            .or_else(|| {
                e.strip_prefix("ansible_facts[")
                    .and_then(|r| r.strip_suffix(']'))
                    .and_then(literal)
            })
            .or_else(|| {
                e.strip_prefix("ansible_")
                    .filter(|k| k.bytes().all(is_word))
            });
        if let Some(key) = fact {
            return NAME_FACTS.contains(&key).then(|| vec![ANY.to_string()]);
        }
        self.values(e)?
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|s| !Templar::is_template(s) && !s.contains(ANY))
                    .map(str::to_string)
            })
            .collect()
    }

    /// Every value the variable `name` is given, when all of them are known.
    fn values(&self, name: &str) -> Option<&Vec<Value>> {
        let word = !name.is_empty() && name.bytes().all(is_word);
        if word && !self.defs_unknown {
            self.defs.get(name)
        } else {
            None
        }
    }

    /// The names a loop item stands for: a literal list's strings, or the strings of every list a
    /// bare variable is given.
    fn items(&self, task: &PlayTask, depth: usize) -> Option<Vec<String>> {
        if task.loop_with.is_some() {
            return None;
        }
        let lists: Vec<&Vec<Value>> = match task.loop_items.as_ref()? {
            Value::Array(items) => vec![items],
            Value::String(s) => self
                .values(bare_variable(s)?)?
                .iter()
                .map(Value::as_array)
                .collect::<Option<_>>()?,
            _ => return None,
        };
        let mut out = Vec::new();
        for item in lists.into_iter().flatten() {
            let strings: Vec<&Value> = match item {
                Value::Array(inner) if task.with_items => inner.iter().collect(),
                other => vec![other],
            };
            for s in strings {
                out.extend(self.names(s.as_str()?, task, depth + 1)?);
            }
        }
        Some(out)
    }

    /// Whether the task's loop is a list proven empty: a literal `[]`, or a variable every value
    /// of which is `[]`.
    fn loop_is_empty(&self, task: &PlayTask) -> bool {
        if task.loop_with.is_some() {
            return false;
        }
        match &task.loop_items {
            Some(Value::Array(items)) => items.is_empty(),
            Some(Value::String(s)) => bare_variable(s)
                .and_then(|name| self.values(name))
                .is_some_and(|values| {
                    !values.is_empty()
                        && values
                            .iter()
                            .all(|v| v.as_array().is_some_and(Vec::is_empty))
                }),
            _ => false,
        }
    }

    /// Every file under `dir`, at any depth.
    fn tree(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                self.tree(&path);
            } else {
                self.file(&path);
            }
        }
    }

    fn text(&mut self, text: &str, place: &str) {
        for expr in expressions(text) {
            let first = expr
                .trim_start_matches(['-', '+'])
                .split_whitespace()
                .next();
            if let Some(pull @ ("include" | "import" | "from" | "extends")) = first {
                self.dynamic(&format!("{{% {pull} %}}"), place);
            }
            self.expression(expr, place);
        }
    }

    fn task(&mut self, task: &PlayTask, origin: &Origin, place: &str) {
        let (text, bare) = task_strings(task);
        for s in text {
            self.text(s, place);
        }
        for s in bare {
            self.expression(s, place);
        }
        if let Some(lookup) = task.loop_with.as_deref()
            && DYNAMIC_LOOKUPS.contains(&short_name(lookup))
        {
            self.dynamic(&format!("with_{lookup}"), place);
        }
        for (name, value) in &task.vars {
            self.define(name, value);
        }
        if let Some(register) = &task.register {
            self.define(register, &Value::Null);
        }
        if task.loop_var != "item" {
            self.define(&task.loop_var, &Value::Null);
        }
        let literal = |key: &str| task.args.get(key).and_then(Value::as_str);
        match short_name(&task.module) {
            // The action plugins that read a fact themselves.
            "package" | "dnf" => self.key("pkg_mgr"),
            "service" => self.key("service_mgr"),
            "reboot" => {
                for key in ["distribution", "distribution_version", "os_family"] {
                    self.key(key);
                }
            }
            "set_fact" => {
                for (name, value) in &task.args {
                    match name.as_str() {
                        "cacheable" => {}
                        "_raw_params" => self.defs_unknown = true,
                        _ => self.define(name, value),
                    }
                }
            }
            "template" => match literal("src") {
                Some(src) if Templar::is_template(src) => {
                    self.later(Wanted::Template, src, task, origin, place);
                }
                Some(src) => {
                    if !self.find(origin, "templates", src) {
                        self.dynamic(&format!("template {src}, not found"), place);
                    }
                }
                None => self.dynamic("template with no literal src", place),
            },
            "include_vars" => {
                if let Some(name) = literal("name") {
                    self.define(name, &Value::Null);
                }
                match literal("file").or_else(|| literal("_raw_params")) {
                    Some(name) if Templar::is_template(name) => {
                        self.later(Wanted::Vars, name, task, origin, place);
                    }
                    Some(name) => {
                        if !self.find(origin, "vars", name) {
                            self.dynamic(&format!("include_vars {name}, not found"), place);
                        }
                    }
                    None => self.dynamic("include_vars", place),
                }
            }
            "include_tasks" => match literal("file").or_else(|| literal("_raw_params")) {
                Some(name) if Templar::is_template(name) => {
                    self.later(Wanted::Tasks, name, task, origin, place);
                }
                Some(_) => {}
                None => self.dynamic("include_tasks", place),
            },
            "include_role" => match literal("name").or_else(|| literal("role")) {
                Some(name) if !Templar::is_template(name) => {}
                _ => self.dynamic("include_role", place),
            },
            _ => {}
        }
    }

    /// One Jinja expression: what sits between the braces, or a bare condition.
    fn expression(&mut self, e: &str, place: &str) {
        let b = e.as_bytes();
        let mut i = 0;
        // The quote character of the string literal the scan is in, and where that literal opens.
        let mut quote: Option<(u8, usize)> = None;
        while i < b.len() {
            let c = b[i];
            match quote {
                Some(_) if c == b'\\' => {
                    i += 2;
                    continue;
                }
                Some((q, _)) if c == q => {
                    quote = None;
                    i += 1;
                    continue;
                }
                None if c == b'\'' || c == b'"' => {
                    if let Some(lit) = leading_literal(&e[i..]) {
                        let lookup = e[..i]
                            .trim_end()
                            .strip_suffix('(')
                            .map(str::trim_end)
                            .is_some_and(|f| {
                                ["lookup", "query", "q"].iter().any(|name| {
                                    f.strip_suffix(name).is_some_and(|b| {
                                        !b.bytes().last().is_some_and(|c| is_word(c) || c == b'.')
                                    })
                                })
                            });
                        if lookup && DYNAMIC_LOOKUPS.contains(&short_name(lit)) {
                            self.dynamic(&format!("lookup('{lit}')"), place);
                        }
                    }
                    quote = Some((c, i));
                    i += 1;
                    continue;
                }
                _ => {}
            }
            if (c.is_ascii_alphabetic() || c == b'_') && (i == 0 || !is_word(b[i - 1])) {
                let start = i;
                while i < b.len() && is_word(b[i]) {
                    i += 1;
                }
                self.word(e, start, i, quote, place);
                continue;
            }
            i += 1;
        }
    }

    fn word(&mut self, e: &str, start: usize, end: usize, quote: Option<(u8, usize)>, place: &str) {
        let word = &e[start..end];
        if word == "ansible_facts" {
            return self.facts(e, start, end, quote, place);
        }
        // Puppet's and Chef's facts, which the Python `setup` adds without a prefix when their
        // tool is on the host, and the native never collects.
        if word.starts_with("facter_") || word.starts_with("ohai_") {
            return self.key(word);
        }
        if let Some(key) = word.strip_prefix("ansible_") {
            if key.is_empty() {
                self.dynamic("'ansible_' joined to a name", place);
            } else if !not_fact(key) {
                self.key(key);
            }
            return;
        }
        if quote.is_some() || (start > 0 && e.as_bytes()[start - 1] == b'.') {
            return;
        }
        match word {
            "vars" => self.dynamic("vars", place),
            "hostvars" => self.hostvars(e, start, end, place),
            _ => {}
        }
    }

    /// `ansible_facts` at `start..end`.
    fn facts(
        &mut self,
        e: &str,
        start: usize,
        end: usize,
        quote: Option<(u8, usize)>,
        place: &str,
    ) {
        let rest = &e[end..];
        let Some((q, open)) = quote else {
            return self.facts_access(rest, place);
        };
        // A dotted path in a string, as `selectattr` and `map(attribute=)` take one.
        if rest.starts_with('.') {
            return self.facts_access(rest, place);
        }
        // `hostvars[h]['ansible_facts'][...]`, or `extract`'s list `['ansible_facts', 'x']`.
        let whole = open + 1 == start && rest.as_bytes().first() == Some(&q);
        if whole && e[..open].trim_end().ends_with('[') {
            let after = rest[1..].trim_start();
            if let Some(next) = after.strip_prefix(']') {
                return self.facts_access(next, place);
            }
            if let Some(key) = after
                .strip_prefix(',')
                .and_then(|next| leading_literal(next.trim_start()))
            {
                return self.key(key);
            }
        }
        self.dynamic("'ansible_facts' as a whole", place);
    }

    /// What follows `ansible_facts`: `.name` or `['name']` read one key, anything else all.
    fn facts_access(&mut self, rest: &str, place: &str) {
        if let Some(after) = rest.strip_prefix('.') {
            let name = leading_word(after);
            if !name.is_empty() && !after[name.len()..].starts_with('(') {
                return self.key(name);
            }
            return self.dynamic(&format!("ansible_facts.{name}("), place);
        }
        if rest.starts_with('[') {
            match close(rest, 0) {
                Some(end) => match literal(&rest[1..end]) {
                    Some(key) => self.key(key),
                    None => self.dynamic(&format!("ansible_facts{}", &rest[..=end]), place),
                },
                None => self.dynamic("ansible_facts[", place),
            }
            return;
        }
        self.dynamic("ansible_facts", place);
    }

    /// `hostvars` at `start..end`, outside a string. One variable of one host is read by name
    /// (the fact names in it are read by the string and word rules); a host's whole set, or
    /// `hostvars` itself, reads every fact.
    fn hostvars(&mut self, e: &str, start: usize, end: usize, place: &str) {
        let rest = &e[end..];
        if rest.starts_with('[') {
            let Some(host_end) = close(rest, 0) else {
                return self.dynamic("hostvars[", place);
            };
            let after = &rest[host_end + 1..];
            if let Some(a) = after.strip_prefix('.') {
                let name = leading_word(a);
                if name.is_empty() || a[name.len()..].starts_with('(') {
                    self.dynamic(&format!("hostvars[...].{name}("), place);
                }
                return;
            }
            if after.starts_with('[') {
                if !close(after, 0).is_some_and(|c| literal(&after[1..c]).is_some()) {
                    self.dynamic("hostvars[...][<expression>]", place);
                }
                return;
            }
            return self.dynamic(&format!("hostvars{} used whole", &rest[..=host_end]), place);
        }
        // `map('extract', hostvars, <key>)`, and `map('extract', hostvars)` whose hosts are only
        // looked into by `selectattr`/`rejectattr` and then `map(attribute=)`.
        let before = e[..start].trim_end();
        let extract = before
            .strip_suffix(',')
            .map(str::trim_end)
            .is_some_and(|b| b.ends_with("'extract'") || b.ends_with("\"extract\""));
        if extract {
            let after = rest.trim_start();
            if let Some(key) = after.strip_prefix(',').map(str::trim_start) {
                let listed = key.starts_with('[')
                    && close(key, 0)
                        .is_some_and(|c| key[1..c].split(',').all(|k| literal(k).is_some()));
                if leading_literal(key).is_some() || listed {
                    return;
                }
                return self.dynamic("map('extract', hostvars, <expression>)", place);
            }
            if after.strip_prefix(')').is_some_and(attributes_only) {
                return;
            }
        }
        self.dynamic("hostvars used whole", place);
    }
}

/// Whether what follows `map('extract', hostvars)` only looks into each host by a literal
/// attribute: any number of `selectattr('...', ...)`/`rejectattr('...', ...)`, then
/// `map(attribute='...')`. An attribute named by a variable could name any fact.
fn attributes_only(chain: &str) -> bool {
    let mut s = chain.trim_start();
    loop {
        let Some(next) = s.strip_prefix('|').map(str::trim_start) else {
            return false;
        };
        if next.starts_with("selectattr(") || next.starts_with("rejectattr(") {
            let open = next.find('(').unwrap_or_default();
            let Some(end) = close(next, open) else {
                return false;
            };
            if !literal_first(&next[open + 1..end]) {
                return false;
            }
            s = next[end + 1..].trim_start();
            continue;
        }
        return next
            .strip_prefix("map(")
            .and_then(|m| m.trim_start().strip_prefix("attribute="))
            .is_some_and(literal_first);
    }
}

/// Whether `args` opens with a string literal that ends the argument.
fn literal_first(args: &str) -> bool {
    let args = args.trim_start();
    leading_literal(args).is_some_and(|lit| {
        let after = args[lit.len() + 2..].trim_start();
        after.is_empty() || after.starts_with(',') || after.starts_with(')')
    })
}

/// The variable `s` is when it is `{{ name }}` and nothing else.
fn bare_variable(s: &str) -> Option<&str> {
    let name = s.trim().strip_prefix("{{")?.strip_suffix("}}")?.trim();
    (!name.is_empty() && name.bytes().all(is_word)).then_some(name)
}

/// Every file matching `pattern`, where [`ANY`] in a component stands for any run of characters.
fn glob(pattern: &Path) -> Vec<PathBuf> {
    let mut found = vec![PathBuf::new()];
    for component in pattern.components() {
        let part = component.as_os_str().to_string_lossy();
        if !part.contains(ANY) {
            found.iter_mut().for_each(|f| f.push(component));
            continue;
        }
        found = found
            .iter()
            .filter_map(|dir| std::fs::read_dir(dir).ok())
            .flatten()
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| wild_match(&part, &n.to_string_lossy()))
            })
            .collect();
    }
    found.retain(|p| p.is_file());
    found.sort();
    found
}

/// Whether `name` matches `pattern`, [`ANY`] matching any run of characters.
fn wild_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split(ANY).collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if parts.len() == 1 {
        return pattern == name;
    }
    if name.len() < first.len() + last.len() || !name.starts_with(first) || !name.ends_with(last) {
        return false;
    }
    let (mut at, end) = (first.len(), name.len() - last.len());
    for middle in &parts[1..parts.len() - 1] {
        match name[at..end].find(middle) {
            Some(i) => at += i + middle.len(),
            None => return false,
        }
    }
    true
}

fn not_fact(key: &str) -> bool {
    NOT_FACTS.contains(&key)
        || NOT_FACT_PREFIXES.iter().any(|p| key.starts_with(p))
        || key.ends_with("_interpreter")
        || (key.starts_with("ssh_") && !key.starts_with("ssh_host_key_"))
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn leading_word(s: &str) -> &str {
    let end = s.bytes().position(|c| !is_word(c)).unwrap_or(s.len());
    &s[..end]
}

/// The content of the string literal `s` opens with.
fn leading_literal(s: &str) -> Option<&str> {
    let q = *s.as_bytes().first()?;
    if q != b'\'' && q != b'"' {
        return None;
    }
    let end = s[1..].find(q as char)?;
    let lit = &s[1..=end];
    (!lit.contains('\\')).then_some(lit)
}

/// The content of `s` when `s` is one string literal and nothing else.
fn literal(s: &str) -> Option<&str> {
    let s = s.trim();
    leading_literal(s).filter(|lit| lit.len() + 2 == s.len())
}

/// The index of the bracket closing the one at `open`, strings skipped.
fn close(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut i = open;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 1;
            } else if c == q {
                quote = None;
            }
        } else {
            match c {
                b'\'' | b'"' => quote = Some(c),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree for one test, removed when it goes out of scope.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let dir = std::env::temp_dir()
                .join(format!("volant-facts-read-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Tree(dir)
        }

        fn file(&self, path: &str, text: &str) -> &Self {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
            std::fs::write(path, text).expect("a file");
            self
        }

        /// Every play of `site.yml`, compiled with `roles/` beside it, read under `inventory`.
        fn read(&self, inventory: &str) -> FactsRead {
            let pb =
                crate::playbook::load(&self.0.join("site.yml")).unwrap_or_else(|e| panic!("{e:#}"));
            let search = crate::roles::RoleSearch {
                paths: vec![self.0.join("roles")],
                collections: Vec::new(),
            };
            let compiled: Vec<Compiled> = pb
                .plays
                .iter()
                .map(|play| {
                    crate::compile::compile(play, &search, &crate::compile::TagSelection::default())
                        .unwrap_or_else(|e| panic!("{e:#}"))
                })
                .collect();
            let walked: Vec<_> = pb
                .plays
                .iter()
                .map(|play| play.dir.as_path())
                .zip(&compiled)
                .collect();
            let reach = Reach::walk(&walked).unwrap_or_else(|e| panic!("{e:#}"));
            let inventory =
                crate::inventory::Inventory::parse_ini(inventory).expect("an inventory");
            let store = VarStore::new(&inventory, None, &self.0, Map::new()).expect("a var store");
            let plays: Vec<(&Play, &Compiled)> = pb.plays.iter().zip(&compiled).collect();
            facts_read(&plays, &reach, &store)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn keys(read: &FactsRead) -> &BTreeSet<String> {
        match read {
            FactsRead::Keys(keys) => keys,
            FactsRead::All(why) => panic!("every fact, for {why}"),
        }
    }

    fn set(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| (*k).to_string()).collect()
    }

    /// One task file holding `when` over `expr`, in a play of its own.
    fn read_one(name: &str, expr: &str) -> FactsRead {
        let tree = Tree::new(name);
        tree.file(
            "site.yml",
            &format!(
                "- hosts: all\n  tasks:\n    - name: t\n      debug:\n        msg: \"{{{{ {expr} }}}}\"\n"
            ),
        );
        tree.read("h1\n")
    }

    /// The four Galaxy roles of `just bench`, cut down to every line that reads a fact, as
    /// published (geerlingguy.security 3.0.2, nginx 3.3.1, git 3.0.1, pip 3.1.2): conditions
    /// on `ansible_facts.os_family` and on the bare `ansible_os_family`, `include_vars` and
    /// `include_tasks` of templated names inside the role, a template whose `src` is a variable,
    /// a role default reading the processor count, the `package` and `service` plugins, and
    /// `ansible_facts['python']` by subscript.
    ///
    /// What would make this red: the role's templates or defaults left unread (`processor_*`
    /// goes), the plugins' own reads forgotten (`pkg_mgr`, `service_mgr`), a templated name
    /// inside a role taken as unread (every fact), or a connection variable such as
    /// `ansible_user` taken for a fact.
    #[test]
    fn the_proof_roles_read_only_native_facts() {
        let tree = Tree::new("roles");
        tree.file(
            "site.yml",
            "- hosts: targets\n  become: true\n  roles:\n    - geerlingguy.security\n    - geerlingguy.nginx\n    - geerlingguy.git\n    - geerlingguy.pip\n",
        )
        .file(
            "roles/geerlingguy.security/tasks/main.yml",
            "- name: Include OS-specific variables.\n  include_vars: \"{{ ansible_facts.os_family }}.yml\"\n\
             - include_tasks: fail2ban.yml\n  when: security_fail2ban_enabled | bool\n\
             - include_tasks: autoupdate-Debian.yml\n  when:\n    - ansible_facts.os_family == 'Debian'\n    - security_autoupdate_enabled | bool\n",
        )
        .file(
            "roles/geerlingguy.security/tasks/fail2ban.yml",
            "- name: Install fail2ban (Debian).\n  package: name=fail2ban state=present\n  when: ansible_facts.os_family == 'Debian'\n\
             - name: Copy fail2ban custom configuration file into place.\n  template:\n    src: \"{{ security_fail2ban_custom_configuration_template }}\"\n    dest: /etc/fail2ban/jail.local\n\
             - name: Ensure fail2ban is running and enabled on boot.\n  service:\n    name: fail2ban\n    state: started\n",
        )
        .file(
            "roles/geerlingguy.security/tasks/autoupdate-Debian.yml",
            "- name: Copy unattended-upgrades configuration files in place.\n  template:\n    src: \"{{ item }}.j2\"\n    dest: \"/etc/apt/apt.conf.d/{{ item }}\"\n  with_items:\n    - 10periodic\n",
        )
        .file(
            "roles/geerlingguy.security/templates/jail.local.j2",
            "[sshd]\n{% if ansible_facts.os_family == 'Debian' and ansible_facts.distribution_major_version | int >= 12 %}\nbackend = systemd\n{% endif %}\n",
        )
        .file(
            "roles/geerlingguy.security/templates/10periodic.j2",
            "APT::Periodic::Update-Package-Lists \"1\";\nAPT::Periodic::Unattended-Upgrade \"1\";\n",
        )
        .file(
            "roles/geerlingguy.security/defaults/main.yml",
            "security_fail2ban_custom_configuration_template: \"jail.local.j2\"\n",
        )
        .file("roles/geerlingguy.security/vars/Debian.yml", "security_ssh_config_path: /etc/ssh/sshd_config\n")
        .file(
            "roles/geerlingguy.nginx/defaults/main.yml",
            "nginx_conf_template: \"nginx.conf.j2\"\nnginx_vhost_template: \"vhost.j2\"\nnginx_vhosts: []\nnginx_worker_processes: >-\n  \"{{ ansible_facts.processor_vcpus | default(ansible_facts.processor_count) }}\"\n",
        )
        .file(
            "roles/geerlingguy.nginx/tasks/main.yml",
            "- name: Include OS-specific variables.\n  include_vars: \"{{ ansible_facts.os_family }}.yml\"\n\
             - include_tasks: setup-Ubuntu.yml\n  when: ansible_facts.distribution == 'Ubuntu'\n\
             - name: Copy nginx configuration in place.\n  template:\n    src: \"{{ nginx_conf_template }}\"\n    dest: \"{{ nginx_conf_file_path }}\"\n\
             - import_tasks: vhosts.yml\n",
        )
        .file(
            "roles/geerlingguy.nginx/tasks/vhosts.yml",
            "- name: Add managed vhost config files.\n  template:\n    src: \"{{ item.template|default(nginx_vhost_template) }}\"\n    dest: \"{{ nginx_vhost_path }}/{{ item.filename }}\"\n  when: item.state|default('present') != 'absent'\n  with_items: \"{{ nginx_vhosts }}\"\n",
        )
        .file(
            "roles/geerlingguy.nginx/tasks/setup-Ubuntu.yml",
            "- name: Ensure nginx is installed.\n  apt:\n    name: \"{{ nginx_package_name }}\"\n    state: present\n",
        )
        .file(
            "roles/geerlingguy.nginx/templates/nginx.conf.j2",
            "user  {{ nginx_user }};\nworker_processes  {{ nginx_worker_processes }};\n# {{ ansible_managed }}\n",
        )
        .file(
            "roles/geerlingguy.git/tasks/main.yml",
            "- name: Ensure git is installed (Debian).\n  apt:\n    name: \"{{ git_packages }}\"\n    state: present\n  when: ansible_os_family == 'Debian'\n\
             - name: Include OS-specific variables (Fedora).\n  include_vars: \"{{ ansible_distribution }}.yml\"\n  when: ansible_distribution == \"Fedora\"\n",
        )
        .file(
            "roles/geerlingguy.pip/tasks/main.yml",
            "- name: Remove the EXTERNALLY-MANAGED file.\n  file:\n    path: \"/usr/lib/python3.{{ ansible_facts['python']['version']['minor'] }}/EXTERNALLY-MANAGED\"\n    state: absent\n\
             - name: Ensure pip is installed.\n  package:\n    name: \"{{ pip_package }}\"\n    state: present\n",
        );
        let read = tree.read(
            "[targets]\ntarget ansible_host=target ansible_python_interpreter=/usr/bin/python3\n",
        );
        assert_eq!(
            keys(&read),
            &set(&[
                "distribution",
                "distribution_major_version",
                "os_family",
                "pkg_mgr",
                "processor_count",
                "processor_vcpus",
                "python",
                "service_mgr",
            ])
        );
        assert!(native_gather_allowed(&read));
        assert_eq!(
            decide(&read),
            (Facts::Native, "native (keys: 8)".to_string())
        );
    }

    /// The k3s-ansible playbook of `just bench-k3s` (pinned commit), cut down to every line that
    /// reads a fact: the `lsb` and distribution conditions, `ansible_facts['service_mgr']`, the
    /// firewalld source list that runs every host through `map('extract', hostvars)` and looks
    /// into each by attribute, `ansible_facts.packages` and `.services` (written by
    /// `package_facts` and `service_facts`, never by `setup`), the nftables template reading
    /// `hostvars[host].ansible_default_ipv4`, the token read off the first server, and the
    /// Raspberry Pi role's `include_tasks` over `with_first_found`, whose terms name a `set_fact`
    /// variable and facts.
    ///
    /// What would make this red: `extract` with no key taken for a whole read, the attribute
    /// paths in strings left unread (`default_ipv4` goes), the template body unread, or the
    /// `package_facts` keys counted as `setup`'s.
    #[test]
    fn the_k3s_playbook_reads_only_native_facts() {
        let tree = Tree::new("k3s");
        tree.file(
            "site.yml",
            "- name: Cluster prep\n  hosts: k3s_cluster\n  gather_facts: true\n  roles:\n    - role: prereq\n    - role: raspberrypi\n\
             - name: Setup K3S server\n  hosts: server\n  gather_facts: false\n  roles:\n    - role: k3s_server\n",
        )
        .file(
            "roles/raspberrypi/tasks/main.yml",
            "- name: Run Raspberry Pi-specific tasks\n  when:\n    - raspberrypi_grep_cpuinfo.rc == 0\n  block:\n\
             \x20   - name: Set detected_distribution to Debian\n      ansible.builtin.set_fact:\n        detected_distribution: Debian\n      when: >\n        ansible_facts.lsb.id|default(\"\") == \"Debian\"\n\
             \x20   - name: Execute OS related tasks on the Raspberry Pi\n      ansible.builtin.include_tasks: \"{{ item }}\"\n      with_first_found:\n        - \"prereq/{{ detected_distribution }}.yml\"\n        - \"prereq/{{ ansible_facts['distribution'] }}-{{ ansible_facts['distribution_major_version'] }}.yml\"\n        - \"prereq/{{ ansible_facts['distribution'] }}.yml\"\n        - \"prereq/default.yml\"\n",
        )
        .file(
            "roles/raspberrypi/tasks/prereq/Debian.yml",
            "- name: Check if /boot/firmware/cmdline.txt exists\n  ansible.builtin.stat:\n    path: /boot/firmware/cmdline.txt\n  register: raspberrypi_boot_file\n",
        )
        .file("roles/raspberrypi/tasks/prereq/default.yml", "---\n")
        .file(
            "roles/prereq/tasks/main.yml",
            "- name: Set same timezone on every Server\n  community.general.timezone:\n    name: \"{{ system_timezone }}\"\n  when: (system_timezone is defined) and (system_timezone != \"Your/Timezone\")\n\
             - name: Populate service facts\n  ansible.builtin.service_facts:\n\
             - name: Gather package facts\n  ansible.builtin.package_facts:\n\
             - name: Allow UFW Exceptions\n  when:\n    - ansible_facts.services['ufw'] is defined\n    - ansible_facts.services['ufw'].state == 'running'\n  debug:\n    msg: ufw\n\
             - name: Add kernel module\n  when: \"'iptables' in ansible_facts.packages\"\n  debug:\n    msg: nft\n\
             - name: Allow nodes\n  when: ansible_facts.services['firewalld.service'] is defined\n  ansible.posix.firewalld:\n    source: \"{{ item }}\"\n    zone: internal\n  loop: >-\n    {{\n      (\n        groups[server_group] | default([])\n        + groups[agent_group] | default([])\n      )\n      | map('extract', hostvars)\n      | selectattr('ansible_facts.default_ipv4', 'defined')\n      | map(attribute='ansible_facts.default_ipv4.address')\n      | flatten | unique | list\n    }}\n\
             - name: Add nftables rules\n  ansible.builtin.template:\n    src: k3s.nft.j2\n    dest: /etc/nftables.d/k3s.nft\n\
             - name: Enable IPv6 forwarding\n  ansible.posix.sysctl:\n    name: net.ipv6.conf.all.forwarding\n    value: \"1\"\n  when: ansible_facts['all_ipv6_addresses']\n\
             - name: Add br_netfilter\n  when: ansible_facts['os_family'] == \"RedHat\" or ansible_facts['distribution'] == \"Archlinux\"\n  debug:\n    msg: x\n\
             - name: Check lsb\n  when: ansible_facts.lsb.id == 'Ubuntu' and ansible_facts['distribution_version'] is version('22.04', '>=')\n  debug:\n    msg: x\n\
             - name: Download k3s binary\n  ansible.builtin.get_url:\n    url: https://github.com/k3s-io/k3s/releases/download/{{ k3s_version }}/k3s{{ k3s_arch_suffix[ansible_facts['architecture']] }}\n    dest: /usr/local/bin/k3s\n  when: ansible_facts['service_mgr'] == 'systemd' and ansible_facts['kernel'] is defined\n\
             - name: Rename host\n  when: ansible_facts['hostname'] != inventory_hostname and ansible_check_mode is false\n  debug:\n    msg: \"{{ ansible_user }}@{{ ansible_host }}:{{ ansible_port | default(22) }} {{ ansible_connection }} {{ ansible_become }} {{ ansible_version.full }} {{ ansible_run_tags }}\"\n",
        )
        .file(
            "roles/prereq/templates/k3s.nft.j2",
            "{% for host in (groups[server_group] | default([]) + groups[agent_group] | default([])) | unique %}\n\
             {% if hostvars[host].ansible_default_ipv4 is defined %}\n\
             insert rule inet filter input ip saddr {{ hostvars[host].ansible_default_ipv4.address }} accept\n\
             {% endif %}\n{% endfor %}\n",
        )
        .file(
            "roles/k3s_server/tasks/main.yml",
            "- name: Add token\n  when: token is not defined and hostvars[groups[server_group][0]].random_token is defined\n  ansible.builtin.set_fact:\n    k3s_server_config: \"{{ k3s_server_config | combine({'token': hostvars[groups[server_group][0]].random_token}) }}\"\n",
        );
        let read = tree.read(
            "[server]\ntarget ansible_host=target\n[agent]\ngen ansible_host=gen\n[k3s_cluster:children]\nserver\nagent\n\
             [k3s_cluster:vars]\nansible_user=user\napi_endpoint=\"{{ hostvars[groups['server'][0]]['ansible_host'] | default(groups['server'][0]) }}\"\n",
        );
        assert_eq!(
            keys(&read),
            &set(&[
                "all_ipv6_addresses",
                "architecture",
                "default_ipv4",
                "distribution",
                "distribution_major_version",
                "distribution_version",
                "hostname",
                "kernel",
                "lsb",
                "os_family",
                "service_mgr",
            ])
        );
        assert!(native_gather_allowed(&read));
    }

    /// Review Focus 4: a fact read by a name only known once it renders reads any fact, and the
    /// Python `setup` stays.
    ///
    /// What would make this red: one of these constructs read as a set of keys.
    #[test]
    fn a_dynamic_fact_read_keeps_python_facts() {
        for (name, expr) in [
            ("subscript", "ansible_facts[item]"),
            ("to-json", "hostvars[h] | to_json"),
            ("hostvars-alone", "hostvars | dict2items"),
            ("vars", "vars['ansible_' ~ x]"),
            ("vars-name", "vars[fact_name]"),
            ("lookup", "lookup('vars', 'ansible_' ~ x)"),
            ("query", "query('ansible.builtin.vars', 'y')"),
            ("template", "lookup('template', 'x.j2')"),
            ("whole", "ansible_facts | to_json"),
            ("get", "ansible_facts.get('mounts')"),
            (
                "extract-expr",
                "groups.all | map('extract', hostvars, item)",
            ),
            (
                "extract-whole",
                "groups.all | map('extract', hostvars) | map('to_json')",
            ),
            (
                "extract-facts",
                "groups.all | map('extract', hostvars, 'ansible_facts')",
            ),
            ("host-subscript", "hostvars[h][name]"),
            ("items", "hostvars[h].items()"),
            (
                "extract-attr-var",
                "groups.all | map('extract', hostvars) | map(attribute=addr_attr) | list",
            ),
            (
                "extract-select-var",
                "groups.all | map('extract', hostvars) | selectattr(fact_var, 'defined') | map(attribute='x')",
            ),
            (
                "legacy-template",
                "lookup('ansible.legacy.template', 'report.j2')",
            ),
            ("legacy-vars", "query('ansible.legacy.vars', fact_name)"),
            ("lookup-space", "lookup ('vars', fact_name)"),
        ] {
            let read = read_one(name, expr);
            assert!(matches!(read, FactsRead::All(_)), "{expr}: {read:?}");
            assert!(!native_gather_allowed(&read), "{expr}");
            assert_eq!(decide(&read).0, Facts::Python, "{expr}");
        }
    }

    /// The literal forms each read one key: attribute, subscript, another host's by name and by
    /// `extract` with a key, in its list form too.
    ///
    /// What would make this red: any of them read as every fact, or as the wrong key.
    #[test]
    fn a_literal_fact_read_is_one_key() {
        for (name, expr, key) in [
            ("attr", "ansible_facts.mounts[0].size", "mounts"),
            ("subscript", "ansible_facts['devices']", "devices"),
            ("bare", "ansible_kernel_version", "kernel_version"),
            ("sub-key", "ansible_default_ipv4.address", "default_ipv4"),
            (
                "other-host",
                "hostvars['h2']['ansible_facts']['fqdn']",
                "fqdn",
            ),
            (
                "other-host-attr",
                "hostvars[h].ansible_facts.domain",
                "domain",
            ),
            (
                "extract",
                "groups.all | map('extract', hostvars, 'ansible_machine')",
                "machine",
            ),
            (
                "extract-list",
                "groups.all | map('extract', hostvars, ['ansible_facts', 'nodename'])",
                "nodename",
            ),
        ] {
            assert_eq!(keys(&read_one(name, expr)), &set(&[key]), "{expr}");
        }
    }

    /// A key the native does not collect keeps the Python `setup`, and the profile names it.
    ///
    /// What would make this red: the check against `NATIVE_FACT_KEYS` skipped.
    #[test]
    fn a_key_outside_the_native_keeps_python_facts() {
        let read = read_one("mounts", "ansible_mounts | map(attribute='mount')");
        assert_eq!(keys(&read), &set(&["mounts"]));
        assert!(!native_gather_allowed(&read));
        assert_eq!(
            decide(&read),
            (
                Facts::Python,
                "python (mounts read, not collected natively)".to_string()
            )
        );
    }

    /// Where the text is: a template with a literal `src` outside any role, a `vars_files`
    /// entry, a play variable, a `group_vars` file, and an `include_vars` file.
    ///
    /// What would make this red: any of those sources left unread, which drops its key.
    #[test]
    fn every_source_of_text_is_read() {
        let tree = Tree::new("sources");
        tree.file(
            "site.yml",
            concat!(
                "- hosts: all\n",
                "  vars:\n",
                "    a: \"{{ ansible_fqdn }}\"\n",
                "  vars_files:\n",
                "    - vars/extra.yml\n",
                "  tasks:\n",
                "    - template:\n",
                "        src: conf.j2\n",
                "        dest: /etc/conf\n",
                "    - include_vars: more.yml\n",
            ),
        )
        .file("templates/conf.j2", "{{ ansible_mounts }}\n")
        .file("vars/extra.yml", "b: \"{{ ansible_devices }}\"\n")
        .file("vars/more.yml", "c: \"{{ ansible_lvm }}\"\n")
        .file(
            "group_vars/all.yml",
            "d: \"{{ ansible_uptime_seconds }}\"\n",
        );
        let read = tree.read("h1\n");
        assert_eq!(
            keys(&read),
            &set(&["devices", "fqdn", "lvm", "mounts", "uptime_seconds"])
        );
    }

    /// What the scan cannot open reads every fact: a templated name outside a role, a template
    /// that is not there, a template pulling another in, and a role named by a template.
    ///
    /// What would make this red: any of those passed over.
    #[test]
    fn what_the_scan_cannot_open_reads_every_fact() {
        for (name, task) in [
            (
                "src",
                "template:\n        src: \"{{ x }}.j2\"\n        dest: /x",
            ),
            (
                "missing",
                "template:\n        src: missing.j2\n        dest: /x",
            ),
            ("pull", "template:\n        src: pulls.j2\n        dest: /x"),
            ("include", "include_tasks: \"{{ x }}.yml\""),
            ("role", "include_role:\n        name: \"{{ x }}\""),
            ("vars", "include_vars: \"{{ x }}.yml\""),
            (
                "path-fact",
                "include_vars: \"{{ ansible_user_dir }}/vars.yml\"",
            ),
        ] {
            let tree = Tree::new(name);
            tree.file(
                "site.yml",
                &format!("- hosts: all\n  tasks:\n    - {task}\n"),
            )
            .file("templates/pulls.j2", "{% include 'other.j2' %}\n");
            let read = tree.read("h1\n");
            assert!(matches!(read, FactsRead::All(_)), "{name}: {read:?}");
        }
    }

    /// Puppet's and Chef's facts, which the Python `setup` adds without the `ansible_` prefix,
    /// are keys the native never collects.
    ///
    /// What would make this red: only `ansible_` words counted as facts.
    #[test]
    fn facter_and_ohai_facts_keep_python_facts() {
        for (name, expr, key) in [
            ("facter", "facter_virtual == 'kvm'", "facter_virtual"),
            ("ohai", "ohai_platform", "ohai_platform"),
        ] {
            let read = read_one(name, expr);
            assert_eq!(keys(&read), &set(&[key]), "{expr}");
            assert!(!native_gather_allowed(&read), "{expr}");
        }
    }

    /// A role whose templated file name renders outside the role: to a path built from
    /// `playbook_dir`, to a name found only in the playbook's `templates/`, and to a task file the
    /// walk never read.
    ///
    /// What would make this red: a templated name inside a role taken as one of the role's own
    /// files without its values known and followed.
    #[test]
    fn a_templated_name_leaving_the_role_is_followed() {
        let role = |tree: &Tree| {
            tree.file("site.yml", "- hosts: all\n  roles:\n    - web\n")
                .file(
                    "roles/web/defaults/main.yml",
                    "conf: nginx.conf.j2\nhook: none.yml\n",
                )
                .file(
                    "roles/web/templates/nginx.conf.j2",
                    "{{ ansible_hostname }}\n",
                )
                .file("roles/web/tasks/none.yml", "- debug: msg=x\n")
                .file(
                    "roles/web/tasks/main.yml",
                    "- template:\n    src: \"{{ conf }}\"\n    dest: /etc/nginx.conf\n\
                     - include_tasks: \"{{ hook }}\"\n",
                );
        };
        let tree = Tree::new("leave-playbook-dir");
        role(&tree);
        tree.file(
            "group_vars/all.yml",
            "conf: \"{{ playbook_dir }}/templates/nginx.conf.j2\"\n",
        );
        assert!(matches!(tree.read("h1\n"), FactsRead::All(_)));

        let tree = Tree::new("leave-by-name");
        role(&tree);
        tree.file("group_vars/all.yml", "conf: custom.j2\n").file(
            "templates/custom.j2",
            "{% if ansible_virtualization_type == 'lxc' %}x{% endif %}\n",
        );
        let read = tree.read("h1\n");
        assert_eq!(keys(&read), &set(&["hostname", "virtualization_type"]));
        assert!(!native_gather_allowed(&read));

        let tree = Tree::new("leave-hook");
        role(&tree);
        let hook = tree.0.join("hooks/extra.yml");
        tree.file("hooks/extra.yml", "- debug: msg=\"{{ ansible_mounts }}\"\n")
            .file("group_vars/all.yml", &format!("hook: {}\n", hook.display()));
        assert!(matches!(tree.read("h1\n"), FactsRead::All(_)));

        let tree = Tree::new("leave-hook-templated");
        role(&tree);
        tree.file("hooks/extra.yml", "- debug: msg=\"{{ ansible_mounts }}\"\n")
            .file(
                "group_vars/all.yml",
                "hook: \"{{ playbook_dir }}/hooks/extra.yml\"\n",
            );
        assert!(matches!(tree.read("h1\n"), FactsRead::All(_)));

        // A variable named by a template can be any variable.
        let tree = Tree::new("leave-set-fact");
        role(&tree);
        tree.file(
            "roles/web/tasks/pick.yml",
            "- set_fact:\n    \"{{ which }}\": \"{{ playbook_dir }}/t.j2\"\n",
        );
        assert!(matches!(tree.read("h1\n"), FactsRead::All(_)));

        // `with_first_found` finds a file in the playbook's `files/` before the role's own.
        let tree = Tree::new("leave-first-found");
        role(&tree);
        tree.file(
            "roles/web/tasks/os.yml",
            "- include_tasks: \"{{ item }}\"\n  with_first_found:\n    - \"prereq/{{ ansible_facts['distribution'] }}.yml\"\n    - prereq/default.yml\n",
        )
        .file("roles/web/tasks/prereq/default.yml", "- debug: msg=x\n")
        .file("files/prereq/Ubuntu.yml", "- debug: msg=\"{{ ansible_mounts }}\"\n");
        assert!(matches!(tree.read("h1\n"), FactsRead::All(_)));

        // Known values alone: a name read, a fact standing for any name, followed out of the
        // role to the playbook's `vars/`.
        let tree = Tree::new("stay");
        role(&tree);
        tree.file("vars/Debian.yml", "x: \"{{ ansible_lsb }}\"\n")
            .file(
                "roles/web/tasks/more.yml",
                "- include_vars: \"{{ ansible_facts.os_family }}.yml\"\n",
            );
        assert_eq!(
            keys(&tree.read("h1\n")),
            &set(&["hostname", "lsb", "os_family"])
        );
    }

    /// A block's own keywords in a file only the walk reaches (a dynamic include here) are read.
    ///
    /// What would make this red: `flatten` keeping a block's tasks and dropping its `when` and
    /// `vars`.
    #[test]
    fn a_block_keyword_in_an_included_file_is_read() {
        let tree = Tree::new("block");
        tree.file("site.yml", "- hosts: all\n  roles:\n    - base\n")
            .file(
                "roles/base/tasks/main.yml",
                "- include_tasks: \"setup-{{ ansible_os_family }}.yml\"\n",
            )
            .file(
                "roles/base/tasks/setup-Debian.yml",
                "- block:\n    - debug: msg=x\n  when: ansible_virtualization_type != 'docker'\n  vars:\n    disk: \"{{ ansible_devices }}\"\n",
            );
        let read = tree.read("h1\n");
        assert_eq!(
            keys(&read),
            &set(&["devices", "os_family", "virtualization_type"])
        );
    }

    /// A play handler's include is followed, and what the included file reads counts.
    ///
    /// What would make this red: the walk taking only the handlers' module names.
    #[test]
    fn a_play_handler_include_is_read() {
        let tree = Tree::new("handler");
        tree.file(
            "site.yml",
            "- hosts: all\n  tasks:\n    - debug: msg=x\n  handlers:\n    - name: reconfigure\n      include_tasks: handlers/reconfigure.yml\n",
        )
        .file(
            "handlers/reconfigure.yml",
            "- template:\n    src: fstab.j2\n    dest: /etc/fstab\n",
        )
        .file("templates/fstab.j2", "{% for m in ansible_mounts %}{{ m }}{% endfor %}\n");
        let read = tree.read("h1\n");
        assert_eq!(keys(&read), &set(&["mounts"]));
    }
}
