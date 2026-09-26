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

use std::collections::BTreeSet;
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

/// Every fact name the run's plays can read: in the text of every task and handler they reach,
/// every template and vars file in the roles they reach, the body of a template with a literal
/// `src`, a literal `include_vars` file and `vars_files` entry, role parameters, play `vars:`,
/// the inventory and its `group_vars` and `host_vars`, and the command line's variables.
///
/// A role is read whole, used or not: every file under its `templates/`, `vars/`, `defaults/`
/// and `meta/`, and every task file. That is what lets a templated `src`, `include_vars` or
/// `include_tasks` inside a role count as read when its name is a plain file name.
pub(crate) fn facts_read(
    plays: &[(&Play, &Compiled)],
    reach: &Reach,
    store: &VarStore,
) -> FactsRead {
    let mut scan = Scan {
        base: store.playbook_dir().to_path_buf(),
        keys: BTreeSet::new(),
        all: None,
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
        let mut texts = Vec::new();
        for value in map.values() {
            strings(value, &mut texts);
        }
        for text in texts {
            self.text(text, place);
        }
    }

    /// A file's text, read as a template: a vars file's values and a template body alike. A file
    /// that is not there or not text reads nothing.
    fn file(&mut self, path: &Path) {
        if let Ok(text) = std::fs::read_to_string(path) {
            let place = self.shown(path);
            self.text(&text, &place);
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
        let literal = |key: &str| task.args.get(key).and_then(Value::as_str);
        // A templated name inside a role, with no directory in it, names a file of the role,
        // and every file of a role is read.
        let in_role = |name: &str| origin.role_dir.is_some() && !name.contains('/');
        match short_name(&task.module) {
            // The action plugins that read a fact themselves.
            "package" | "dnf" => self.key("pkg_mgr"),
            "service" => self.key("service_mgr"),
            "reboot" => {
                for key in ["distribution", "distribution_version", "os_family"] {
                    self.key(key);
                }
            }
            "template" => match literal("src") {
                Some(src) if Templar::is_template(src) => {
                    if !in_role(src) {
                        self.dynamic(&format!("template src {src}"), place);
                    }
                }
                Some(src) => {
                    let found = crate::action_plugins::files::search_paths(
                        origin,
                        &self.base,
                        "templates",
                        src,
                    )
                    .into_iter()
                    .find(|p| p.is_file());
                    match found {
                        Some(path) => self.file(&path),
                        None => self.dynamic(&format!("template {src}, not found"), place),
                    }
                }
                None => self.dynamic("template with no literal src", place),
            },
            "include_vars" => match literal("file").or_else(|| literal("_raw_params")) {
                Some(name) if in_role(name) => {}
                Some(name) if !Templar::is_template(name) => {
                    let found = crate::action_plugins::files::search_paths(
                        origin, &self.base, "vars", name,
                    )
                    .into_iter()
                    .find(|p| p.is_file());
                    match found {
                        Some(path) => self.file(&path),
                        None => self.dynamic(&format!("include_vars {name}, not found"), place),
                    }
                }
                _ => self.dynamic("include_vars", place),
            },
            "include_tasks" => match literal("file").or_else(|| literal("_raw_params")) {
                Some(name) if !Templar::is_template(name) || in_role(name) => {}
                _ => self.dynamic("include_tasks", place),
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
                        let before = e[..i].trim_end();
                        let lookup = ["lookup(", "query(", "q("]
                            .iter()
                            .any(|f| before.ends_with(f));
                        let name = lit.strip_prefix("ansible.builtin.").unwrap_or(lit);
                        if lookup && DYNAMIC_LOOKUPS.contains(&name) {
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

/// Whether what follows `map('extract', hostvars)` only looks into each host by attribute: any
/// number of `selectattr(...)`/`rejectattr(...)`, then `map(attribute=...)`.
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
            s = next[end + 1..].trim_start();
            continue;
        }
        return next
            .strip_prefix("map(")
            .is_some_and(|m| m.trim_start().starts_with("attribute="));
    }
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
            let reach = Reach::walk(&compiled.iter().collect::<Vec<_>>())
                .unwrap_or_else(|e| panic!("{e:#}"));
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
        .file("roles/geerlingguy.security/vars/Debian.yml", "security_ssh_config_path: /etc/ssh/sshd_config\n")
        .file(
            "roles/geerlingguy.nginx/defaults/main.yml",
            "nginx_conf_template: \"nginx.conf.j2\"\nnginx_worker_processes: >-\n  \"{{ ansible_facts.processor_vcpus | default(ansible_facts.processor_count) }}\"\n",
        )
        .file(
            "roles/geerlingguy.nginx/tasks/main.yml",
            "- name: Include OS-specific variables.\n  include_vars: \"{{ ansible_facts.os_family }}.yml\"\n\
             - include_tasks: setup-Ubuntu.yml\n  when: ansible_facts.distribution == 'Ubuntu'\n\
             - name: Copy nginx configuration in place.\n  template:\n    src: \"{{ nginx_conf_template }}\"\n    dest: \"{{ nginx_conf_file_path }}\"\n",
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
    /// `hostvars[host].ansible_default_ipv4`, and the token read off the first server.
    ///
    /// What would make this red: `extract` with no key taken for a whole read, the attribute
    /// paths in strings left unread (`default_ipv4` goes), the template body unread, or the
    /// `package_facts` keys counted as `setup`'s.
    #[test]
    fn the_k3s_playbook_reads_only_native_facts() {
        let tree = Tree::new("k3s");
        tree.file(
            "site.yml",
            "- name: Cluster prep\n  hosts: k3s_cluster\n  gather_facts: true\n  roles:\n    - role: prereq\n\
             - name: Setup K3S server\n  hosts: server\n  gather_facts: false\n  roles:\n    - role: k3s_server\n",
        )
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
}
