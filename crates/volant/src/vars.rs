// SPDX-License-Identifier: GPL-3.0-or-later
//! Variable sources and their precedence, from inventory groups up to extra vars, plus the
//! magic variables Ansible defines for every host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use crate::inventory::Inventory;
use crate::yaml;

/// What a play and a task contribute on top of inventory-level sources.
#[derive(Debug, Default, Clone)]
pub struct Scope {
    pub play_vars: Map<String, Value>,
    pub vars_files: Vec<Map<String, Value>>,
    pub task_vars: Map<String, Value>,
    /// Hosts of the current play, in inventory order.
    pub play_hosts: Vec<String>,
}

/// Every variable source that does not depend on the play, resolved once per run, plus the
/// facts hosts accumulate (`set_fact`, `register`) while the run goes.
#[derive(Debug)]
pub struct VarStore {
    inventory_dir: Option<PathBuf>,
    inventory_file: Option<PathBuf>,
    playbook_dir: PathBuf,
    /// Group vars from `group_vars/` directories: inventory-side then playbook-side.
    group_files: [BTreeMap<String, Map<String, Value>>; 2],
    /// Host vars from `host_vars/` directories: inventory-side then playbook-side.
    host_files: [BTreeMap<String, Map<String, Value>>; 2],
    inventory_vars: BTreeMap<String, Map<String, Value>>,
    host_line_vars: BTreeMap<String, Map<String, Value>>,
    group_names: BTreeMap<String, Vec<String>>,
    groups: BTreeMap<String, Vec<String>>,
    facts: BTreeMap<String, Map<String, Value>>,
    extra: Map<String, Value>,
    /// What `ansible_forks` reports, which the reference sets from the run's own `forks`.
    forks: usize,
    /// Every inventory host's view, as `hostvars` shows it. Built on demand and dropped
    /// whenever something below it changes, which only a fact or a rebase does.
    hostvars: Option<Map<String, Value>>,
}

/// The value Ansible substitutes for `omit`: a parameter equal to it is dropped from the task.
/// Ansible generates a fresh random suffix per run; a stable one changes nothing for playbooks
/// and keeps output reproducible.
pub fn omit_token() -> &'static str {
    "__omit_place_holder__6d24c2dd6a8f2e0e2a6e5cce6b1f9c4a"
}

impl VarStore {
    pub fn new(
        inventory: &Inventory,
        inventory_path: Option<&Path>,
        playbook_dir: &Path,
        extra: Map<String, Value>,
    ) -> anyhow::Result<Self> {
        let inventory_dir = inventory_path
            .and_then(|p| p.parent())
            .map(Path::to_path_buf);
        let roots: [Option<&Path>; 2] = [inventory_dir.as_deref(), Some(playbook_dir)];
        let mut group_files = [BTreeMap::new(), BTreeMap::new()];
        let mut host_files = [BTreeMap::new(), BTreeMap::new()];
        for (i, root) in roots.iter().enumerate() {
            if let Some(root) = root {
                group_files[i] = load_vars_dir(&root.join("group_vars"))?;
                host_files[i] = load_vars_dir(&root.join("host_vars"))?;
            }
        }
        let groups = inventory.groups();
        let mut inventory_vars = BTreeMap::new();
        let mut host_line_vars = BTreeMap::new();
        let mut group_names = BTreeMap::new();
        for host in &groups["all"] {
            // Not `resolve`: a name that is both a group and a host resolves as the group, and
            // an empty group would leave this host without its own inventory variables.
            let resolved = inventory.host_with_vars(host);
            inventory_vars.insert(host.clone(), resolved.vars.into_iter().collect());
            host_line_vars.insert(
                host.clone(),
                inventory
                    .host_line_vars(host)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
            );
            group_names.insert(host.clone(), inventory.group_names_of(host));
        }
        Ok(Self {
            inventory_dir,
            inventory_file: inventory_path.map(Path::to_path_buf),
            playbook_dir: playbook_dir.to_path_buf(),
            group_files,
            host_files,
            inventory_vars,
            host_line_vars,
            group_names,
            groups,
            facts: BTreeMap::new(),
            extra,
            forks: crate::config::DEFAULT_FORKS,
            hostvars: None,
        })
    }

    /// What `ansible_forks` reports for this run.
    pub fn set_forks(&mut self, forks: usize) {
        self.forks = forks;
        self.hostvars = None;
    }

    pub fn playbook_dir(&self) -> &Path {
        &self.playbook_dir
    }

    /// Moves the playbook-side roots to another playbook's directory, keeping the facts hosts
    /// have gathered so far. Inventory-side sources do not move.
    pub fn rebase(&mut self, playbook_dir: &Path) -> anyhow::Result<()> {
        self.group_files[1] = load_vars_dir(&playbook_dir.join("group_vars"))?;
        self.host_files[1] = load_vars_dir(&playbook_dir.join("host_vars"))?;
        self.playbook_dir = playbook_dir.to_path_buf();
        self.hostvars = None;
        Ok(())
    }

    pub fn set_fact(&mut self, host: &str, key: &str, value: Value) {
        self.facts
            .entry(host.to_string())
            .or_default()
            .insert(key.to_string(), value);
        self.hostvars = None;
    }

    /// The merged view for one host, lowest precedence first: inventory `all`, `group_vars/all`
    /// (inventory then playbook), inventory groups by depth and name, `group_vars/<group>`
    /// (inventory then playbook), inventory host vars, `host_vars/<host>` (inventory then
    /// playbook), play vars, vars_files, task vars, facts, extra vars, then the magic variables.
    pub fn for_host(&mut self, host: &str, scope: &Scope) -> Map<String, Value> {
        let mut vars = self.host_base(host);
        extend(&mut vars, &scope.play_vars);
        for file in &scope.vars_files {
            extend(&mut vars, file);
        }
        extend(&mut vars, &scope.task_vars);
        if let Some(facts) = self.facts.get(host) {
            extend(&mut vars, facts);
        }
        extend(&mut vars, &self.extra);
        self.add_magic(&mut vars, host, scope);
        vars
    }

    /// Inventory-level sources for one host, without play, facts, extra or magic variables.
    /// This is what other hosts see through `hostvars`.
    fn host_base(&self, host: &str) -> Map<String, Value> {
        let mut vars = Map::new();
        for files in &self.group_files {
            if let Some(all) = files.get("all") {
                extend(&mut vars, all);
            }
        }
        // Inventory group vars and inventory host vars arrive already merged in Ansible's
        // group order from `Inventory::resolve`; `group_vars/<group>` files slot in between,
        // so the merged inventory view is applied first and files for the host's groups after.
        if let Some(inv) = self.inventory_vars.get(host) {
            extend(&mut vars, inv);
        }
        if let Some(names) = self.group_names.get(host) {
            for files in &self.group_files {
                for name in names {
                    if let Some(group) = files.get(name) {
                        extend(&mut vars, group);
                    }
                }
            }
        }
        // Inventory host vars must beat `group_vars/<group>` files: re-apply them on top.
        if let Some(host_only) = self.host_line_vars.get(host) {
            extend(&mut vars, host_only);
        }
        for files in &self.host_files {
            if let Some(h) = files.get(host) {
                extend(&mut vars, h);
            }
        }
        vars
    }

    /// What other hosts see of one host: inventory sources, its facts, then extra vars.
    fn host_view(&self, host: &str) -> Map<String, Value> {
        let mut base = self.host_base(host);
        if let Some(facts) = self.facts.get(host) {
            extend(&mut base, facts);
        }
        extend(&mut base, &self.extra);
        base
    }

    fn hostvars(&mut self) -> &Map<String, Value> {
        if self.hostvars.is_none() {
            let names = self.groups["all"].clone();
            let mut map = Map::new();
            for name in names {
                let view = self.host_view(&name);
                map.insert(name, Value::Object(view));
            }
            self.hostvars = Some(map);
        }
        self.hostvars.as_ref().expect("just built")
    }

    fn add_magic(&mut self, vars: &mut Map<String, Value>, host: &str, scope: &Scope) {
        let mut hostvars = self.hostvars().clone();
        if !hostvars.contains_key(host) {
            // An implicit localhost is in no group, so it is not in the cached map.
            let view = self.host_view(host);
            hostvars.insert(host.to_string(), Value::Object(view));
        }
        let short = host.split('.').next().unwrap_or(host).to_string();
        let group_names = self.group_names.get(host).cloned().unwrap_or_default();
        let insert = |vars: &mut Map<String, Value>, k: &str, v: Value| {
            vars.insert(k.to_string(), v);
        };
        insert(vars, "inventory_hostname", Value::String(host.to_string()));
        insert(vars, "inventory_hostname_short", Value::String(short));
        insert(
            vars,
            "group_names",
            serde_json::to_value(group_names).unwrap_or_default(),
        );
        insert(
            vars,
            "groups",
            serde_json::to_value(&self.groups).unwrap_or_default(),
        );
        insert(vars, "hostvars", Value::Object(hostvars));
        insert(
            vars,
            "ansible_play_hosts",
            serde_json::to_value(&scope.play_hosts).unwrap_or_default(),
        );
        insert(
            vars,
            "ansible_play_hosts_all",
            serde_json::to_value(&scope.play_hosts).unwrap_or_default(),
        );
        insert(
            vars,
            "ansible_play_batch",
            serde_json::to_value(&scope.play_hosts).unwrap_or_default(),
        );
        insert(
            vars,
            "play_hosts",
            serde_json::to_value(&scope.play_hosts).unwrap_or_default(),
        );
        insert(
            vars,
            "playbook_dir",
            Value::String(self.playbook_dir.display().to_string()),
        );
        if let Some(dir) = &self.inventory_dir {
            insert(
                vars,
                "inventory_dir",
                Value::String(dir.display().to_string()),
            );
        }
        if let Some(file) = &self.inventory_file {
            insert(
                vars,
                "inventory_file",
                Value::String(file.display().to_string()),
            );
        }
        insert(vars, "omit", Value::String(omit_token().to_string()));
        insert(vars, "ansible_check_mode", Value::Bool(false));
        insert(vars, "ansible_diff_mode", Value::Bool(false));
        insert(vars, "ansible_forks", Value::from(self.forks));
        insert(vars, "ansible_version", ansible_version());
        insert(
            vars,
            "volant_version",
            Value::String(env!("CARGO_PKG_VERSION").to_string()),
        );
    }
}

/// The ansible-core release Volant reproduces, as the `ansible_version` dictionary roles
/// consult. The number lives in tests/golden/ANSIBLE_VERSION and nowhere else.
pub fn ansible_version() -> Value {
    let full = include_str!("../tests/golden/ANSIBLE_VERSION").trim();
    let mut parts = full.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let (major, minor, revision) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    serde_json::json!({ "full": full, "major": major, "minor": minor, "revision": revision, "string": full })
}

fn extend(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (k, v) in source {
        target.insert(k.clone(), v.clone());
    }
}

/// `group_vars/` or `host_vars/`: for each entry, a file `name`, `name.yml`, `name.yaml`,
/// `name.json`, or a directory `name/` whose files are merged in name order. Hidden files,
/// editor backups and `.retry` files are ignored, like Ansible does.
fn load_vars_dir(dir: &Path) -> anyhow::Result<BTreeMap<String, Map<String, Value>>> {
    let mut out: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if ignored(file_name) {
            continue;
        }
        let name = strip_vars_extension(file_name).to_string();
        let target = out.entry(name).or_default();
        if path.is_dir() {
            let mut inner: Vec<PathBuf> = std::fs::read_dir(&path)?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .collect();
            inner.sort();
            for file in inner.into_iter().filter(|p| p.is_file()) {
                if file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(ignored)
                {
                    continue;
                }
                extend(target, &load_vars_file(&file)?);
            }
        } else {
            extend(target, &load_vars_file(&path)?);
        }
    }
    Ok(out)
}

fn ignored(file_name: &str) -> bool {
    file_name.starts_with('.') || file_name.ends_with('~') || file_name.ends_with(".retry")
}

fn strip_vars_extension(file_name: &str) -> &str {
    for ext in [".yml", ".yaml", ".json"] {
        if let Some(stem) = file_name.strip_suffix(ext) {
            return stem;
        }
    }
    file_name
}

/// A YAML or JSON file whose document is a mapping. An empty file is an empty mapping.
pub fn load_vars_file(path: &Path) -> anyhow::Result<Map<String, Value>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let docs = yaml::load(&text, &path.display().to_string())?;
    match docs.first() {
        None => Ok(Map::new()),
        Some(doc) => match yaml::to_json(doc).with_context(|| format!("in {}", path.display()))? {
            Value::Object(map) => Ok(map),
            Value::Null => Ok(Map::new()),
            _ => bail!(
                "{}: a variables file must contain a mapping",
                path.display()
            ),
        },
    }
}

/// `-e` values: `key=value` pairs (strings), inline JSON or YAML mappings, and `@file`.
/// Later items win over earlier ones.
pub fn parse_extra_vars(items: &[String], base: &Path) -> anyhow::Result<Map<String, Value>> {
    let mut out = Map::new();
    for item in items {
        let item = item.trim();
        if let Some(file) = item.strip_prefix('@') {
            extend(&mut out, &load_vars_file(&base.join(file))?);
        } else if item.starts_with('{') || item.starts_with('[') {
            let docs = yaml::load(item, "extra vars")?;
            match docs.first().map(yaml::to_json).transpose()? {
                Some(Value::Object(map)) => extend(&mut out, &map),
                _ => bail!("extra vars must be a mapping: {item}"),
            }
        } else {
            for word in shlex::split(item).unwrap_or_default() {
                let (k, v) = word
                    .split_once('=')
                    .with_context(|| format!("extra var '{word}' is not key=value"))?;
                out.insert(k.to_string(), Value::String(v.to_string()));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "volant-vars-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(dir.join("inventory/group_vars/web")).unwrap();
        std::fs::create_dir_all(dir.join("inventory/host_vars")).unwrap();
        std::fs::create_dir_all(dir.join("play/group_vars")).unwrap();
        std::fs::create_dir_all(dir.join("play/host_vars")).unwrap();
        std::fs::write(
            dir.join("inventory/hosts.ini"),
            "[web]\nweb1 tier=ini\n[web:vars]\nfrom=ini_group\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("inventory/group_vars/all.yml"),
            "layer: inv_all\nonly_inv_all: 1\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("inventory/group_vars/web/10-first.yml"),
            "layer: inv_web\nsplit: first\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("inventory/group_vars/web/20-second.yaml"),
            "split: second\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("inventory/host_vars/web1.json"),
            r#"{"layer": "inv_host"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("play/group_vars/all"), "play_all: yes\n").unwrap();
        std::fs::write(dir.join("play/host_vars/web1.yml"), "layer: play_host\n").unwrap();
        std::fs::write(dir.join("play/group_vars/.hidden.yml"), "hidden: true\n").unwrap();
        std::fs::write(dir.join("play/group_vars/all.yml~"), "backup: true\n").unwrap();
        dir
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn store(dir: &std::path::Path) -> (Inventory, VarStore) {
        let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
        let store = VarStore::new(
            &inv,
            Some(&dir.join("inventory/hosts.ini")),
            &dir.join("play"),
            Map::new(),
        )
        .unwrap();
        (inv, store)
    }

    fn scope(hosts: &[&str]) -> Scope {
        Scope {
            play_vars: Map::new(),
            vars_files: Vec::new(),
            task_vars: Map::new(),
            play_hosts: hosts.iter().map(|h| h.to_string()).collect(),
        }
    }

    #[test]
    fn directories_layer_in_ansible_order() {
        let dir = tree();
        let (_, mut store) = store(&dir);
        let v = store.for_host("web1", &scope(&["web1"]));
        assert_eq!(
            v["layer"],
            json!("play_host"),
            "playbook host_vars is the highest directory source"
        );
        assert_eq!(v["only_inv_all"], json!(1));
        assert_eq!(
            v["split"],
            json!("second"),
            "files in a directory apply in name order"
        );
        assert_eq!(v["from"], json!("ini_group"));
        assert_eq!(v["tier"], json!("ini"));
        assert_eq!(
            v["play_all"],
            json!(true),
            "yes as data is a boolean too, matching PyYAML"
        );
        assert!(!v.contains_key("hidden") && !v.contains_key("backup"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn play_task_fact_and_extra_vars_stack_on_top() {
        let dir = tree();
        let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
        let extra: Map<String, Value> = json!({"layer": "extra"}).as_object().unwrap().clone();
        let mut store = VarStore::new(
            &inv,
            Some(&dir.join("inventory/hosts.ini")),
            &dir.join("play"),
            extra,
        )
        .unwrap();
        let mut sc = scope(&["web1"]);
        sc.play_vars = json!({"layer": "play", "p": 1})
            .as_object()
            .unwrap()
            .clone();
        sc.vars_files = vec![json!({"p": 2, "f": 1}).as_object().unwrap().clone()];
        sc.task_vars = json!({"f": 2, "t": 1}).as_object().unwrap().clone();
        store.set_fact("web1", "t", json!(2));
        store.set_fact("web1", "fact", json!("set"));
        let v = store.for_host("web1", &sc);
        assert_eq!(v["layer"], json!("extra"), "extra vars beat everything");
        assert_eq!(v["p"], json!(2), "vars_files beat play vars");
        assert_eq!(v["f"], json!(2), "task vars beat vars_files");
        assert_eq!(v["t"], json!(2), "set_fact beats task vars");
        assert_eq!(v["fact"], json!("set"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn magic_variables_are_present() {
        let dir = tree();
        let (_, mut store) = store(&dir);
        let v = store.for_host("web1", &scope(&["web1"]));
        assert_eq!(v["inventory_hostname"], json!("web1"));
        assert_eq!(v["inventory_hostname_short"], json!("web1"));
        assert_eq!(v["group_names"], json!(["web"]));
        assert_eq!(v["groups"]["web"], json!(["web1"]));
        assert_eq!(v["hostvars"]["web1"]["tier"], json!("ini"));
        assert!(
            v["hostvars"]["web1"].get("hostvars").is_none(),
            "hostvars does not nest"
        );
        assert_eq!(v["ansible_play_hosts"], json!(["web1"]));
        assert_eq!(v["ansible_play_hosts_all"], json!(["web1"]));
        assert_eq!(v["ansible_play_batch"], json!(["web1"]));
        assert_eq!(
            v["playbook_dir"],
            json!(dir.join("play").display().to_string())
        );
        assert_eq!(
            v["inventory_dir"],
            json!(dir.join("inventory").display().to_string())
        );
        assert_eq!(
            v["inventory_file"],
            json!(dir.join("inventory/hosts.ini").display().to_string())
        );
        assert_eq!(v["omit"], json!(omit_token()));
        assert_eq!(v["ansible_check_mode"], json!(false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_short_hostname_drops_the_domain() {
        let inv = Inventory::parse_ini("web1.example.com\n").unwrap();
        let mut store = VarStore::new(&inv, None, std::path::Path::new("."), Map::new()).unwrap();
        let v = store.for_host("web1.example.com", &scope(&["web1.example.com"]));
        assert_eq!(v["inventory_hostname_short"], json!("web1"));
    }

    #[test]
    fn a_host_that_shares_a_group_name_still_gets_its_variables() {
        // Ansible warns "Found both group and host with same name" and runs the host.
        let inv = Inventory::parse_ini("[web]\ndb x=1\n[db]\n[web:vars]\nfrom=group\n").unwrap();
        let mut store = VarStore::new(&inv, None, std::path::Path::new("."), Map::new()).unwrap();
        let v = store.for_host("db", &scope(&["db"]));
        assert_eq!(v["inventory_hostname"], json!("db"));
        assert_eq!(v["x"], json!(1));
        assert_eq!(v["from"], json!("group"));
    }

    #[test]
    fn extra_vars_take_key_value_json_and_files() {
        let dir = tree();
        std::fs::write(dir.join("extra.yml"), "from_file: 1\nlayer: file\n").unwrap();
        let parsed = parse_extra_vars(
            &[
                "a=1 b=two".to_string(),
                r#"{"c": [1]}"#.to_string(),
                "@extra.yml".to_string(),
            ],
            &dir,
        )
        .unwrap();
        assert_eq!(parsed["a"], json!("1"), "key=value extra vars are strings");
        assert_eq!(parsed["b"], json!("two"));
        assert_eq!(parsed["c"], json!([1]));
        assert_eq!(parsed["from_file"], json!(1));
        assert_eq!(parsed["layer"], json!("file"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ansible_version_comes_from_the_reference_file() {
        let v = ansible_version();
        let reference = include_str!("../tests/golden/ANSIBLE_VERSION").trim();
        assert_eq!(v["full"], json!(reference));
        assert_eq!(v["major"], json!(2));
    }

    #[test]
    fn a_broken_vars_file_names_itself() {
        let dir = tree();
        std::fs::write(dir.join("play/group_vars/all.yml"), "a: [1,\n").unwrap();
        let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
        let err = VarStore::new(
            &inv,
            Some(&dir.join("inventory/hosts.ini")),
            &dir.join("play"),
            Map::new(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("all.yml"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
