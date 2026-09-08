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
    pub warnings: Vec<String>,
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

    /// Ansible's host pattern grammar: terms separated by `,` or `:`; `all` and `*`; shell
    /// wildcards (`*`, `?`, `[abc]`) on host and group names; `!term` removes, `&term` keeps the
    /// intersection; `name[N]`/`name[N:M]` indexes or slices the terms it follows (`M` inclusive,
    /// as Ansible's `--limit` slicing is, unlike a Rust range). A name that is both a host and a
    /// group means the host, as Ansible's own pattern evaluator checks hosts before groups.
    /// Ansible warns about such a homonym once per run regardless of the pattern used, because the
    /// check runs at inventory load time, not at pattern-resolution time; `resolve` matches that.
    /// `localhost` is implicit with a local connection when the inventory does not define it.
    ///
    /// Terms are not applied in the order they appear: Ansible's own `order_patterns`
    /// (`inventory/manager.py`) sorts them into plain terms first, then `&` terms, then `!` terms,
    /// regardless of how they were interleaved in the pattern text, and — measured — when there is
    /// no plain term at all (`!web`, `&web` alone) it prepends an implicit `all` so the exclusion
    /// or intersection has something to start from, rather than starting from nothing.
    pub fn resolve(&self, pattern: &str) -> Resolution {
        let mut res = Resolution::default();
        for name in self
            .hosts
            .iter()
            .map(|h| h.name.as_str())
            .filter(|n| self.groups.iter().any(|g| g.name == *n))
        {
            res.warnings
                .push(format!("Found both group and host with same name: {name}"));
        }
        let mut regular: Vec<&str> = Vec::new();
        let mut intersects: Vec<&str> = Vec::new();
        let mut excludes: Vec<&str> = Vec::new();
        for term in split_terms(pattern)
            .into_iter()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            match term.chars().next() {
                Some('!') => excludes.push(term[1..].trim()),
                Some('&') => intersects.push(term[1..].trim()),
                _ => regular.push(term),
            }
        }
        if regular.is_empty() {
            regular.push("all");
        }
        let mut selected: Vec<String> = Vec::new();
        for name in regular {
            for h in self.matching(name, &mut res) {
                if !selected.contains(&h) {
                    selected.push(h);
                }
            }
        }
        for name in intersects {
            let keep: HashSet<String> = self.matching(name, &mut res).into_iter().collect();
            selected.retain(|h| keep.contains(h));
        }
        for name in excludes {
            let drop: HashSet<String> = self.matching(name, &mut res).into_iter().collect();
            selected.retain(|h| !drop.contains(h));
        }
        for name in selected {
            res.hosts
                .push(match self.hosts.iter().any(|h| h.name == name) {
                    true => self.host_with_vars(&name),
                    false => implicit_localhost(&name),
                });
        }
        res
    }

    /// Hosts named by one term, in inventory order. Records unmatched terms. A name that exactly
    /// matches a host always means that host, checked before groups and before wildcards, exactly
    /// as Ansible's own `_evaluate_patterns` special-cases an exact host name before ever calling
    /// its group-aware matcher.
    fn matching(&self, term: &str, res: &mut Resolution) -> Vec<String> {
        if let Some((base, sub)) = split_subscript(term) {
            let names = self.matching(base, res);
            return apply_subscript(&names, sub);
        }
        if self.hosts.iter().any(|h| h.name == term) {
            return vec![term.to_string()];
        }
        if term == "all" || term == "*" {
            return self.all_hosts();
        }
        if let Some(g) = self.group(term) {
            return self.group_hosts(g);
        }
        if term.contains(['*', '?', '[']) {
            let mut out: Vec<String> = self
                .hosts
                .iter()
                .map(|h| h.name.clone())
                .filter(|h| glob_match(term, h))
                .collect();
            for g in self.groups.iter().filter(|g| glob_match(term, &g.name)) {
                for h in self.group_hosts(g) {
                    if !out.contains(&h) {
                        out.push(h);
                    }
                }
            }
            if out.is_empty() {
                res.unmatched.push(term.to_string());
            }
            return out;
        }
        if term == "localhost" || term == "127.0.0.1" {
            return vec![term.to_string()];
        }
        res.unmatched.push(term.to_string());
        Vec::new()
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

    /// Hosts of a group and of its descendants, in the order `ansible-inventory` produces: the
    /// group's own hosts first, then each descendant group's own hosts, breadth-first over
    /// `:children`, without duplicates. This is not the inventory's global host order — a group
    /// nested two levels down can list hosts declared earlier in the file than a shallower
    /// sibling, and Ansible's traversal order (not the file's) is what wins.
    fn group_hosts(&self, group: &Group) -> Vec<String> {
        if group.name == "all" {
            return self.all_hosts();
        }
        let children: Vec<&Group> = group
            .children
            .iter()
            .filter_map(|c| self.group(c))
            .collect();
        self.hosts_breadth_first(&group.name, &group.hosts, children)
    }

    /// `all`'s children are implicit: `ungrouped` first, then every other group nobody's
    /// `:children` section lists, in the order those groups were first declared.
    fn all_children(&self) -> Vec<&Group> {
        let mut out = Vec::new();
        if let Some(ungrouped) = self.group("ungrouped") {
            out.push(ungrouped);
        }
        let has_parent: HashSet<&str> = self
            .groups
            .iter()
            .flat_map(|g| g.children.iter().map(String::as_str))
            .collect();
        for g in &self.groups {
            if g.name != "all" && g.name != "ungrouped" && !has_parent.contains(g.name.as_str()) {
                out.push(g);
            }
        }
        out
    }

    /// Every host in the inventory, in `ansible-inventory`'s order for the pattern `all`: not the
    /// order hosts were declared, but a breadth-first walk of the implicit group tree rooted at
    /// `all` (see `all_children`). Measured against the reference: a plain, ungrouped inventory
    /// happens to produce file order, but one with nested `:children` groups does not.
    pub fn all_hosts(&self) -> Vec<String> {
        let own_hosts = self
            .group("all")
            .map(|g| g.hosts.clone())
            .unwrap_or_default();
        self.hosts_breadth_first("all", &own_hosts, self.all_children())
    }

    /// Ansible's `Group._get_hosts`: the root's own hosts, then a breadth-first walk of
    /// `:children`, each group contributing its own hosts (not its descendants') in turn, hosts
    /// deduplicated by first appearance.
    fn hosts_breadth_first(
        &self,
        root_name: &str,
        root_hosts: &[String],
        initial_children: Vec<&Group>,
    ) -> Vec<String> {
        let mut ordered_groups: Vec<&Group> = Vec::new();
        let mut seen_groups: HashSet<&str> = HashSet::from([root_name]);
        for g in &initial_children {
            if seen_groups.insert(g.name.as_str()) {
                ordered_groups.push(g);
            }
        }
        let mut frontier = initial_children;
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for g in &frontier {
                for c in g.children.iter().filter_map(|c| self.group(c)) {
                    if seen_groups.insert(c.name.as_str()) {
                        ordered_groups.push(c);
                        next.push(c);
                    }
                }
            }
            frontier = next;
        }
        let mut hosts = Vec::new();
        let mut seen_hosts: HashSet<&str> = HashSet::new();
        for h in root_hosts
            .iter()
            .chain(ordered_groups.iter().flat_map(|g| g.hosts.iter()))
        {
            if seen_hosts.insert(h.as_str()) {
                hosts.push(h.clone());
            }
        }
        hosts
    }

    /// Group vars from the outermost group to the innermost, then host vars on top.
    pub fn host_with_vars(&self, name: &str) -> Host {
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
            .or_insert_with(|| self.all_hosts());
        out.entry("ungrouped".to_string()).or_default();
        out
    }
}

/// Splits a pattern on `,` and `:` the way Ansible's own pattern splitter does: not inside a
/// `[...]` subscript, so `web[0:1]` stays one term instead of being torn into `web[0` and `1]` at
/// the colon that is part of its range, not a term separator.
fn split_terms(pattern: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    for (i, c) in pattern.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' | ':' if depth <= 0 => {
                terms.push(&pattern[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    terms.push(&pattern[start..]);
    terms
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

/// `fnmatch` as Python does it for host patterns: `*` any run, `?` one character, `[abc]` and
/// `[a-z]` sets, `[!abc]` negated sets. No escaping, like Ansible. Ansible only reaches this for
/// bracket content that isn't a bare `[N]`/`[N:M]` subscript; see `split_subscript`.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|i| go(rest, &t[i..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some(('[', rest)) => {
                let Some(close) = rest.iter().position(|c| *c == ']') else {
                    return false;
                };
                let (set, after) = (&rest[..close], &rest[close + 1..]);
                let Some((first, others)) = t.split_first() else {
                    return false;
                };
                let (negate, set) = match set.split_first() {
                    Some(('!', s)) => (true, s),
                    _ => (false, set),
                };
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        hit |= set[i] <= *first && *first <= set[i + 2];
                        i += 3;
                    } else {
                        hit |= set[i] == *first;
                        i += 1;
                    }
                }
                hit != negate && go(after, others)
            }
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    go(
        &pattern.chars().collect::<Vec<_>>(),
        &text.chars().collect::<Vec<_>>(),
    )
}

/// A single index (`[N]`, `N` may be negative) or an inclusive range (`[N:M]` or the deprecated
/// `[N-M]`, `N` and `M` plain digits, `M` optional meaning "to the end"). Measured against
/// `ansible-core`'s `PATTERN_WITH_SUBSCRIPT` regex: the range's own bounds are never negative in
/// text, only a lone index may be; anything else with a `[` (letters, a bare `:` with no leading
/// digit, `~` regexes) is not a subscript and falls through to `glob_match` instead, exactly as
/// the reference falls through to fnmatch for the same inputs.
enum Subscript {
    Index(i64),
    Range(i64, Option<i64>),
}

fn split_subscript(term: &str) -> Option<(&str, Subscript)> {
    if !term.ends_with(']') {
        return None;
    }
    let open = term.rfind('[')?;
    if open == 0 {
        return None;
    }
    let base = &term[..open];
    let inner = &term[open + 1..term.len() - 1];
    let is_signed_digits = |s: &str| {
        let s = s.strip_prefix('-').unwrap_or(s);
        !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
    };
    if is_signed_digits(inner) {
        return Some((base, Subscript::Index(inner.parse().ok()?)));
    }
    let sep = inner.find([':', '-'])?;
    let (start, end) = (&inner[..sep], &inner[sep + 1..]);
    let is_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if !is_digits(start) || (!end.is_empty() && !is_digits(end)) {
        return None;
    }
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    Some((base, Subscript::Range(start.parse().ok()?, end)))
}

/// Ansible's `_apply_subscript`: a Python-style negative index wraps from the end; a range's
/// missing end means "to the last host" and, unlike a Rust or Python range, its given end is
/// inclusive. Anything out of bounds is empty, not an error — the reference does not fail a
/// playbook run over an over-large subscript, it just selects nothing.
fn apply_subscript(names: &[String], sub: Subscript) -> Vec<String> {
    let len = names.len() as i64;
    let wrap = |i: i64| if i < 0 { i + len } else { i };
    match sub {
        Subscript::Index(i) => {
            let i = wrap(i);
            if (0..len).contains(&i) {
                vec![names[i as usize].clone()]
            } else {
                Vec::new()
            }
        }
        Subscript::Range(start, end) => {
            let start = start.clamp(0, len);
            let stop = (end.unwrap_or(len - 1) + 1).clamp(0, len);
            if start >= stop {
                Vec::new()
            } else {
                names[start as usize..stop as usize].to_vec()
            }
        }
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

/// An integer the way Python's `ast.literal_eval` accepts one: optional sign, `_` allowed only
/// between digits (never leading, trailing, or doubled), and no leading zero unless the whole
/// literal is `0`. Measured against the reference: `leading=010` in an INI stays the string
/// `"010"` (Python's grammar rejects a leading zero as a syntax error, so `literal_eval` raises
/// and the ini plugin falls back to the raw text), while `grouped=1_000` becomes the integer
/// `1000` (`_` is a valid digit separator in Python's integer literals since 3.6).
fn python_int(t: &str) -> Option<i64> {
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    if clean.len() > 1 && clean.starts_with('0') {
        return None;
    }
    let value: i64 = clean.parse().ok()?;
    Some(if t.starts_with('-') { -value } else { value })
}

/// What Ansible's INI plugin does with a value: `ast.literal_eval`, falling back to the text.
/// Integers, floats, `True`/`False`/`None`, quoted strings, and JSON-shaped lists and dicts
/// are recognised; everything else, `yes` included, stays a string.
fn literal(text: &str) -> Value {
    let t = text.trim();
    if let Some(i) = python_int(t) {
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

    #[test]
    fn glob_matching_follows_fnmatch() {
        assert!(
            glob_match("web*", "web12")
                && glob_match("web?", "web1")
                && !glob_match("web?", "web12")
        );
        assert!(glob_match("web[12]", "web2") && !glob_match("web[12]", "web3"));
        assert!(glob_match("web[!1]", "web2") && !glob_match("web[!1]", "web1"));
        assert!(glob_match("w[a-z]b1", "web1") && !glob_match("w[a-z]b1", "w3b1"));
        assert!(
            !glob_match("web[", "web["),
            "an unclosed set matches nothing"
        );
    }

    #[test]
    fn negation_and_intersection_compose() {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        assert_eq!(names(&inv.resolve("all:!web").hosts), ["lonely", "db1"]);
        assert_eq!(names(&inv.resolve("prod:&web").hosts), ["web1", "web2"]);
        assert_eq!(names(&inv.resolve("prod:&web:!web2").hosts), ["web1"]);
        assert_eq!(
            names(&inv.resolve("!web").hosts),
            ["lonely", "db1"],
            "a bare exclusion has nothing to start from, so Ansible starts it from all"
        );
    }

    #[test]
    fn a_homonym_means_the_host_and_warns() {
        let inv = Inventory::parse_ini("[same]\nsame\nother\n").unwrap();
        let res = inv.resolve("same");
        assert_eq!(names(&res.hosts), ["same"]);
        assert_eq!(res.warnings.len(), 1);
    }

    #[test]
    fn wildcards_also_match_group_names() {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        assert_eq!(names(&inv.resolve("w*b1").hosts), ["web1"]);
        assert_eq!(names(&inv.resolve("pro*").hosts), ["web1", "web2", "db1"]);
    }

    #[test]
    fn subscripts_index_and_slice_a_resolved_pattern() {
        let inv = Inventory::parse_ini(SAMPLE).unwrap();
        assert_eq!(names(&inv.resolve("web[0]").hosts), ["web1"]);
        assert_eq!(names(&inv.resolve("web[-1]").hosts), ["web2"]);
        assert_eq!(names(&inv.resolve("web[0:1]").hosts), ["web1", "web2"]);
        assert!(
            inv.resolve("web[12]").hosts.is_empty(),
            "an out-of-range index selects nothing, as the reference does, not an error"
        );
    }

    /// The reference's own answer for a value ambiguous in Rust, taken from the inventory golden
    /// rather than retyped, so the two never drift apart.
    #[test]
    fn ini_integers_follow_pythons_literal_grammar() {
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/expected_inventory.json")).unwrap();
        let web1 = &expected["hostvars"]["web1"];
        let inv = Inventory::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden/inventory.ini")
                .as_path(),
        )
        .unwrap();
        let vars = inv.host_with_vars("web1").vars;
        assert_eq!(
            vars["leading"], web1["leading"],
            "a leading zero stays text"
        );
        assert_eq!(
            vars["grouped"], web1["grouped"],
            "an underscore digit separator still parses"
        );
    }
}
