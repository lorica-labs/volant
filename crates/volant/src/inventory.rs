// SPDX-License-Identifier: GPL-3.0-or-later
//! Static INI inventories, the way `ansible-inventory` reads them.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::Path;

use anyhow::Context;
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Host {
    pub name: String,
    pub vars: BTreeMap<String, Value>,
}

#[derive(Debug, Default)]
struct Group {
    name: String,
    hosts: Vec<String>,
    children: Vec<String>,
    vars: BTreeMap<String, Value>,
}

#[derive(Debug, Default)]
pub struct Inventory {
    hosts: Vec<Host>,
    groups: Vec<Group>,
}

#[derive(Debug, Default)]
pub struct Resolution {
    pub hosts: Vec<Host>,
    pub unmatched: Vec<String>,
}

#[derive(Debug)]
pub struct InventoryError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for InventoryError {}

enum Section {
    Hosts(String),
    Children(String),
    Vars(String),
}

impl Inventory {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading inventory {}", path.display()))?;
        Self::parse_ini(&text).with_context(|| format!("parsing inventory {}", path.display()))
    }

    pub fn parse_ini(text: &str) -> Result<Self, InventoryError> {
        let mut inv = Inventory::default();
        let mut section = Section::Hosts("ungrouped".to_string());
        for (idx, raw) in text.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                let header = line
                    .strip_prefix('[')
                    .and_then(|l| l.strip_suffix(']'))
                    .ok_or_else(|| InventoryError {
                        line: line_no,
                        message: "unbalanced section header".to_string(),
                    })?;
                section = match header.rsplit_once(':') {
                    Some((name, "children")) => Section::Children(name.to_string()),
                    Some((name, "vars")) => Section::Vars(name.to_string()),
                    Some((_, kind)) => {
                        return Err(InventoryError {
                            line: line_no,
                            message: format!("unknown section kind '{kind}'"),
                        });
                    }
                    None => Section::Hosts(header.to_string()),
                };
                inv.group_mut(section_name(&section));
                continue;
            }
            match &section {
                Section::Hosts(group) => {
                    let words = shlex_words(line, line_no)?;
                    let (name, pairs) = words
                        .split_first()
                        .expect("non-empty line has a first word");
                    if name.is_empty() {
                        return Err(InventoryError {
                            line: line_no,
                            message: "empty host name".to_string(),
                        });
                    }
                    let vars = key_values(pairs, line_no)?;
                    inv.add_host(name, vars);
                    let group = group.clone();
                    let members = &mut inv.group_mut(&group).hosts;
                    if !members.iter().any(|h| h == name) {
                        members.push(name.to_string());
                    }
                }
                Section::Children(group) => {
                    let group = group.clone();
                    inv.group_mut(line);
                    inv.group_mut(&group).children.push(line.to_string());
                }
                Section::Vars(group) => {
                    let words = shlex_words(line, line_no)?;
                    let vars = key_values(&words, line_no)?;
                    let group = group.clone();
                    inv.group_mut(&group).vars.extend(vars);
                }
            }
        }
        Ok(inv)
    }

    /// Resolves a host pattern: names separated by `,` or `:`, each one `all`, a group or a host.
    /// `localhost` is implicit with a local connection when the inventory does not define it.
    pub fn resolve(&self, pattern: &str) -> Resolution {
        let mut res = Resolution::default();
        let mut seen = HashSet::new();
        for term in pattern
            .split([',', ':'])
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let names: Vec<String> = if term == "all" || term == "*" {
                self.hosts.iter().map(|h| h.name.clone()).collect()
            } else if let Some(group) = self.groups.iter().find(|g| g.name == term) {
                self.group_hosts(group)
            } else if self.hosts.iter().any(|h| h.name == term) {
                vec![term.to_string()]
            } else if term == "localhost" || term == "127.0.0.1" {
                if seen.insert(term.to_string()) {
                    res.hosts.push(implicit_localhost(term));
                }
                continue;
            } else {
                res.unmatched.push(term.to_string());
                continue;
            };
            for name in names {
                if seen.insert(name.clone()) {
                    res.hosts.push(self.host_with_vars(&name));
                }
            }
        }
        res
    }

    fn add_host(&mut self, name: &str, vars: BTreeMap<String, Value>) {
        match self.hosts.iter_mut().find(|h| h.name == name) {
            Some(host) => host.vars.extend(vars),
            None => self.hosts.push(Host {
                name: name.to_string(),
                vars,
            }),
        }
    }

    fn group_mut(&mut self, name: &str) -> &mut Group {
        if let Some(pos) = self.groups.iter().position(|g| g.name == name) {
            return &mut self.groups[pos];
        }
        self.groups.push(Group {
            name: name.to_string(),
            ..Group::default()
        });
        self.groups.last_mut().expect("just pushed")
    }

    fn group(&self, name: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.name == name)
    }

    /// Hosts of a group and of its descendants, in inventory order, without duplicates. `all`
    /// holds every host in the inventory, whether or not any host line names it explicitly.
    fn group_hosts(&self, group: &Group) -> Vec<String> {
        if group.name == "all" {
            return self.hosts.iter().map(|h| h.name.clone()).collect();
        }
        let mut members = HashSet::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::from([group]);
        while let Some(g) = queue.pop_front() {
            if !visited.insert(g.name.as_str()) {
                // A group can list a child that (directly or transitively) lists it back;
                // skip a group already walked instead of re-enqueueing it forever.
                continue;
            }
            members.extend(g.hosts.iter().cloned());
            queue.extend(g.children.iter().filter_map(|c| self.group(c)));
        }
        self.hosts
            .iter()
            .filter(|h| members.contains(&h.name))
            .map(|h| h.name.clone())
            .collect()
    }

    /// Group vars from the outermost group to the innermost, then host vars on top.
    fn host_with_vars(&self, name: &str) -> Host {
        let host = self
            .hosts
            .iter()
            .find(|h| h.name == name)
            .cloned()
            .unwrap_or_default();
        let depth = self.group_depths();
        let mut groups: Vec<&Group> = self
            .groups
            .iter()
            .filter(|g| self.group_hosts(g).iter().any(|h| h == name))
            .collect();
        groups.sort_by_key(|g| (depth.get(&g.name).copied().unwrap_or(1), g.name.clone()));
        let mut vars = BTreeMap::new();
        for g in groups {
            vars.extend(g.vars.clone());
        }
        vars.extend(host.vars);
        Host {
            name: host.name,
            vars,
        }
    }

    /// Distance from the top: `all` sits at depth 0 and is not an implicit parent of anything,
    /// so every other group nobody lists as a child still sits at depth 1, their children at 2.
    fn group_depths(&self) -> HashMap<String, usize> {
        let children: HashSet<&str> = self
            .groups
            .iter()
            .flat_map(|g| g.children.iter().map(String::as_str))
            .collect();
        let mut depth = HashMap::new();
        let mut visited = HashSet::new();
        let mut queue: VecDeque<(&Group, usize)> = self
            .groups
            .iter()
            .filter(|g| g.name != "all" && !children.contains(g.name.as_str()))
            .map(|g| (g, 1))
            .collect();
        if let Some(all) = self.group("all") {
            visited.insert(all.name.as_str());
            depth.insert(all.name.clone(), 0);
        }
        while let Some((g, d)) = queue.pop_front() {
            if !visited.insert(g.name.as_str()) {
                // Same guard as `group_hosts`: a cyclic `:children` chain must not requeue.
                continue;
            }
            depth.insert(g.name.clone(), d);
            queue.extend(
                g.children
                    .iter()
                    .filter_map(|c| self.group(c))
                    .map(|c| (c, d + 1)),
            );
        }
        depth
    }

    /// The variables set on the host line itself, before any group is merged in.
    pub fn host_line_vars(&self, name: &str) -> Option<&BTreeMap<String, Value>> {
        self.hosts.iter().find(|h| h.name == name).map(|h| &h.vars)
    }

    /// Groups the host belongs to, sorted, without `all`: Ansible's `group_names`.
    pub fn group_names_of(&self, host: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .groups
            .iter()
            .filter(|g| g.name != "all" && self.group_hosts(g).iter().any(|h| h == host))
            .map(|g| g.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Every group with its member hosts, including `all` and `ungrouped`: Ansible's `groups`.
    pub fn groups(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = self
            .groups
            .iter()
            .map(|g| (g.name.clone(), self.group_hosts(g)))
            .collect();
        out.entry("all".to_string())
            .or_insert_with(|| self.hosts.iter().map(|h| h.name.clone()).collect());
        out.entry("ungrouped".to_string()).or_default();
        out
    }
}

fn section_name(section: &Section) -> &str {
    match section {
        Section::Hosts(n) | Section::Children(n) | Section::Vars(n) => n,
    }
}

fn implicit_localhost(name: &str) -> Host {
    let mut vars = BTreeMap::new();
    vars.insert(
        "ansible_connection".to_string(),
        Value::String("local".into()),
    );
    Host {
        name: name.to_string(),
        vars,
    }
}

fn shlex_words(line: &str, line_no: usize) -> Result<Vec<String>, InventoryError> {
    shlex::split(line)
        .filter(|w| !w.is_empty())
        .ok_or_else(|| InventoryError {
            line: line_no,
            message: "unbalanced quotes".to_string(),
        })
}

fn key_values(words: &[String], line_no: usize) -> Result<BTreeMap<String, Value>, InventoryError> {
    words
        .iter()
        .map(|w| {
            w.split_once('=')
                .map(|(k, v)| (k.to_string(), literal(v)))
                .ok_or_else(|| InventoryError {
                    line: line_no,
                    message: format!("expected key=value, found '{w}'"),
                })
        })
        .collect()
}

/// What Ansible's INI plugin does with a value: `ast.literal_eval`, falling back to the text.
/// Integers, floats, `True`/`False`/`None`, quoted strings, and JSON-shaped lists and dicts
/// are recognised; everything else, `yes` included, stays a string.
fn literal(text: &str) -> Value {
    let t = text.trim();
    if let Ok(i) = t.parse::<i64>() {
        return Value::from(i);
    }
    if t.contains('.')
        && let Ok(f) = t.parse::<f64>()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return Value::Number(n);
    }
    match t {
        "True" => return Value::Bool(true),
        "False" => return Value::Bool(false),
        "None" => return Value::Null,
        _ => {}
    }
    if t.len() >= 2
        && ((t.starts_with('\'') && t.ends_with('\'')) || (t.starts_with('"') && t.ends_with('"')))
    {
        return Value::String(t[1..t.len() - 1].to_string());
    }
    if ((t.starts_with('[') && t.ends_with(']')) || (t.starts_with('{') && t.ends_with('}')))
        && let Ok(v) = serde_json::from_str::<Value>(t)
    {
        return v;
    }
    Value::String(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"
# hosts without a group land in "ungrouped"
lonely ansible_connection=local

[web]
web1 ansible_host=10.0.0.1 role="front end"
web2

[db]
db1

[web:vars]
http_port=80
tier=web

[prod:children]
web
db

[prod:vars]
tier=prod
env=prod
"#;

    fn names(hosts: &[Host]) -> Vec<&str> {
        hosts.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn groups_children_and_vars_are_read() {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        let web = inv.resolve("web");
        assert_eq!(names(&web.hosts), ["web1", "web2"]);
        assert_eq!(web.hosts[0].vars["ansible_host"], json!("10.0.0.1"));
        assert_eq!(web.hosts[0].vars["role"], json!("front end"));
        assert_eq!(web.hosts[0].vars["http_port"], json!(80));
        assert_eq!(web.hosts[1].vars["http_port"], json!(80));
    }

    #[test]
    fn child_group_vars_win_over_parent_group_vars() {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        let web1 = inv.resolve("web1").hosts.remove(0);
        assert_eq!(web1.vars["tier"], json!("web"));
        assert_eq!(web1.vars["env"], json!("prod"));
        let db1 = inv.resolve("db1").hosts.remove(0);
        assert_eq!(db1.vars["tier"], json!("prod"));
    }

    #[test]
    fn all_and_patterns_resolve_in_inventory_order() {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        assert_eq!(
            names(&inv.resolve("all").hosts),
            ["lonely", "web1", "web2", "db1"]
        );
        assert_eq!(names(&inv.resolve("prod").hosts), ["web1", "web2", "db1"]);
        assert_eq!(names(&inv.resolve("db,web1").hosts), ["db1", "web1"]);
        assert_eq!(names(&inv.resolve("web:db").hosts), ["web1", "web2", "db1"]);
    }

    #[test]
    fn unknown_patterns_are_reported_not_fatal() {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        let res = inv.resolve("nope,web2");
        assert_eq!(names(&res.hosts), ["web2"]);
        assert_eq!(res.unmatched, ["nope"]);
    }

    #[test]
    fn localhost_is_implicit_and_local() {
        let inv = Inventory::empty();
        let res = inv.resolve("localhost");
        assert_eq!(res.hosts[0].name, "localhost");
        assert_eq!(res.hosts[0].vars["ansible_connection"], json!("local"));
        assert!(
            inv.resolve("all").hosts.is_empty(),
            "implicit localhost is not part of all"
        );
    }

    #[test]
    fn a_defined_localhost_wins_over_the_implicit_one() {
        let inv = Inventory::parse_ini("localhost ansible_connection=ssh\n").unwrap();
        assert_eq!(
            inv.resolve("localhost").hosts[0].vars["ansible_connection"],
            json!("ssh")
        );
    }

    #[test]
    fn syntax_errors_carry_the_line_number() {
        let err = Inventory::parse_ini("[web]\nweb1 role=\"unterminated\n").unwrap_err();
        assert_eq!(err.line, 2);
        let err = Inventory::parse_ini("[web:vars]\nno_equals_sign\n").unwrap_err();
        assert_eq!(err.line, 2);
    }

    #[test]
    fn a_circular_group_resolves_instead_of_hanging() {
        let inv = Inventory::parse_ini("[a:children]\nb\n\n[b:children]\na\n").unwrap();
        let res = inv.resolve("a");
        assert!(res.hosts.is_empty());
        assert!(res.unmatched.is_empty());
    }

    #[test]
    fn empty_host_name_is_rejected() {
        let err = Inventory::parse_ini("[web]\n\"\"\n").unwrap_err();
        assert_eq!(err.line, 2);
    }

    #[test]
    fn unbalanced_section_header_is_rejected() {
        let err = Inventory::parse_ini("[web\nweb1\n").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn ini_values_are_python_literals_when_they_parse_as_such() {
        let inv = Inventory::parse_ini(
            "h port=8080 ratio=0.5 flag=True nothing=None name='quoted text' word=yes list=[1,2] raw=a=b\n",
        )
        .unwrap();
        let v = &inv.resolve("h").hosts[0].vars;
        assert_eq!(v["port"], json!(8080));
        assert_eq!(v["ratio"], json!(0.5));
        assert_eq!(v["flag"], json!(true));
        assert_eq!(v["nothing"], json!(null));
        assert_eq!(v["name"], json!("quoted text"));
        assert_eq!(v["word"], json!("yes"), "yes is not a Python literal");
        assert_eq!(v["list"], json!([1, 2]));
        assert_eq!(
            v["raw"],
            json!("a=b"),
            "the first '=' splits, the rest is the value"
        );
    }

    #[test]
    fn all_vars_apply_to_every_host_and_lose_to_any_group() {
        let inv = Inventory::parse_ini("[all:vars]\nx=1\ny=1\n\n[web]\nweb1\n\n[web:vars]\nx=2\n")
            .unwrap();
        let v = &inv.resolve("web1").hosts[0].vars;
        assert_eq!(v["x"], json!(2));
        assert_eq!(v["y"], json!(1));
    }

    #[test]
    fn groups_of_equal_depth_apply_in_alphabetical_order() {
        // Both groups hold web1 at depth 1: "zeta" is applied after "alpha", so it wins.
        let inv = Inventory::parse_ini(
            "[zeta]\nweb1\n[alpha]\nweb1\n[zeta:vars]\nk=zeta\n[alpha:vars]\nk=alpha\n",
        )
        .unwrap();
        assert_eq!(inv.resolve("web1").hosts[0].vars["k"], json!("zeta"));
    }

    #[test]
    fn group_names_and_groups_are_exposed() {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        assert_eq!(inv.group_names_of("web1"), ["prod", "web"]);
        let groups = inv.groups();
        assert_eq!(groups["web"], ["web1", "web2"]);
        assert_eq!(groups["prod"], ["web1", "web2", "db1"]);
        assert_eq!(groups["all"], ["lonely", "web1", "web2", "db1"]);
        assert_eq!(groups["ungrouped"], ["lonely"]);
    }
}
