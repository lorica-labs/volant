// SPDX-License-Identifier: GPL-3.0-or-later
//! Resolving a templated file name to the names it can take, from the values the scan collected.

use std::path::PathBuf;

use serde_json::Value;

use volant_protocol::modules::short_name;

use super::decide::{ANY, MOST_NAMES, NAME_FACTS};
use super::expr::bare_variable;
use super::lex::{is_word, literal};
use super::scan::{Pending, Scan, Wanted, glob};

use crate::playbook::PlayTask;

use crate::template::Templar;

impl Scan {
    /// Reads every file `p` can name; an error names what could not be pinned down.
    pub(super) fn resolve(&mut self, p: &Pending) -> Result<(), String> {
        let (label, sub) = match p.wanted {
            Wanted::Template => ("template", "templates"),
            Wanted::Vars => ("include_vars", "vars"),
            Wanted::Tasks => ("include_tasks", "tasks"),
        };
        let unknown = || format!("{label} {}", p.name);
        let looped = p.task.loop_with.as_deref().map(short_name) == Some("first_found");
        let lookup = first_found_lookup(&p.name);
        if looped || lookup.is_some() {
            // `"{{ item }}"` over `with_first_found`, or `"{{ lookup('first_found', params) }}"`:
            // whichever file is found first, in a role's `files/` (or the subdirectory of the
            // action: `vars/`, `templates/`), the role, its `tasks/` or the playbook's directory.
            let terms = match lookup {
                Some(var) => self.first_found_variable(var, &p.task, 0),
                None if bare_variable(&p.name) == Some(p.task.loop_var.as_str()) => p
                    .task
                    .loop_items
                    .as_ref()
                    .and_then(|items| self.first_found_names(items, &p.task, 0)),
                None => None,
            }
            .ok_or_else(unknown)?;
            let mut bases: Vec<PathBuf> = Vec::new();
            if let Some(role) = &p.origin.role_dir {
                bases.extend([role.clone(), role.join("tasks")]);
            }
            bases.extend([p.origin.file_dir.clone(), self.base.clone()]);
            let dirs: &[&str] = match p.wanted {
                Wanted::Tasks => &["files"],
                _ => &["files", sub],
            };
            let mut candidates = Vec::new();
            for term in &terms {
                for base in &bases {
                    candidates.extend(dirs.iter().map(|dir| base.join(dir).join(term)));
                    candidates.push(base.join(term));
                }
            }
            if matches!(p.wanted, Wanted::Tasks) {
                return self.all_read(&candidates, label);
            }
            for found in candidates.iter().flat_map(|path| glob(path)) {
                self.file(&found);
            }
            return Ok(());
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
    pub(super) fn all_read(&self, candidates: &[PathBuf], label: &str) -> Result<(), String> {
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

    /// The file names a `first_found` term can stand for, as patterns: a name, split at `,`, `;`
    /// and `:` as the lookup splits it, a list of terms, a mapping of `files` under `paths`, or
    /// a bare variable given any of those.
    pub(super) fn first_found_names(
        &self,
        term: &Value,
        task: &PlayTask,
        depth: usize,
    ) -> Option<Vec<String>> {
        if depth > 2 {
            return None;
        }
        match term {
            Value::String(s) => match bare_variable(s) {
                Some(var) if var != task.loop_var => self.first_found_variable(var, task, depth),
                _ => Some(
                    self.names(s, task, 1)?
                        .iter()
                        .flat_map(|name| name.split([',', ';', ':']))
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .collect(),
                ),
            },
            Value::Array(terms) => {
                let mut out = Vec::new();
                for term in terms {
                    out.extend(self.first_found_names(term, task, depth)?);
                }
                Some(out)
            }
            Value::Object(map) => {
                let list = |key: &str| match map.get(key) {
                    Some(value) => self.first_found_names(value, task, depth + 1),
                    None => Some(Vec::new()),
                };
                let (files, paths) = (list("files")?, list("paths")?);
                if paths.is_empty() {
                    return Some(files);
                }
                Some(
                    paths
                        .iter()
                        .flat_map(|path| files.iter().map(move |file| format!("{path}/{file}")))
                        .collect(),
                )
            }
            _ => None,
        }
    }

    /// The file names every value of the variable `var` stands for as a `first_found` term.
    fn first_found_variable(
        &self,
        var: &str,
        task: &PlayTask,
        depth: usize,
    ) -> Option<Vec<String>> {
        let mut out = Vec::new();
        for value in self.values(var)? {
            out.extend(self.first_found_names(value, task, depth + 1)?);
        }
        Some(out)
    }

    /// Every file name `name` can render to, a [`NAME_FACTS`] value standing as [`ANY`]; `None`
    /// when a part of it is not known: a variable given a templated or non-string value
    /// anywhere, or given none the scan sees, a loop item not from a literal list, any other
    /// expression. A loop proven empty names nothing: the task never runs.
    pub(super) fn names(&self, name: &str, task: &PlayTask, depth: usize) -> Option<Vec<String>> {
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
    pub(super) fn segment(&self, e: &str, task: &PlayTask, depth: usize) -> Option<Vec<String>> {
        if e == task.loop_var {
            return self.items(task, depth);
        }
        let base = name_kept(e);
        let fact = base
            .strip_prefix("ansible_facts.")
            .filter(|k| k.bytes().all(is_word))
            .or_else(|| {
                base.strip_prefix("ansible_facts[")
                    .and_then(|r| r.strip_suffix(']'))
                    .and_then(literal)
            })
            .or_else(|| {
                base.strip_prefix("ansible_")
                    .filter(|k| k.bytes().all(is_word))
            });
        if let Some(key) = fact {
            return NAME_FACTS.contains(&key).then(|| vec![ANY.to_string()]);
        }
        // A value that is another bare variable (`mysql_daemon: "{{ __mysql_daemon }}"`) stands
        // for that variable's values.
        let mut out = Vec::new();
        for value in self.values(e)? {
            let s = value.as_str()?;
            match bare_variable(s) {
                Some(var) if depth < 2 && var != task.loop_var => {
                    out.extend(self.segment(var, task, depth + 1)?);
                }
                _ if Templar::is_template(s) || s.contains(ANY) => return None,
                _ => out.push(s.to_string()),
            }
        }
        Some(out)
    }

    /// Every value the variable `name` is given, when all of them are known.
    pub(super) fn values(&self, name: &str) -> Option<&Vec<Value>> {
        let word = !name.is_empty() && name.bytes().all(is_word);
        if word && !self.defs_unknown {
            self.defs.get(name)
        } else {
            None
        }
    }

    /// The names a loop item stands for: a literal list's strings, or the strings of every list a
    /// bare variable is given.
    pub(super) fn items(&self, task: &PlayTask, depth: usize) -> Option<Vec<String>> {
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
    pub(super) fn loop_is_empty(&self, task: &PlayTask) -> bool {
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
}

/// `e` without the trailing operations that keep a one-name value a name with no `/` in it:
/// `.split('<sep>')[<n>]`, `.lower()`, `.upper()` and the `lower`, `upper`, `string`, `int` and
/// `trim` filters (`ansible_facts.distribution_version.split('.')[0]`, geerlingguy.postgresql).
/// `e` itself when it ends in anything else.
fn name_kept(e: &str) -> &str {
    let mut e = e.trim();
    loop {
        let shorter = e
            .strip_suffix(".lower()")
            .or_else(|| e.strip_suffix(".upper()"))
            .or_else(|| {
                let (head, filter) = e.rsplit_once('|')?;
                matches!(filter.trim(), "lower" | "upper" | "string" | "int" | "trim")
                    .then_some(head)
            })
            .or_else(|| split_index(e));
        match shorter {
            Some(head) => e = head.trim_end(),
            None => return e,
        }
    }
}

/// `head` of `head.split('<sep>')[<n>]`, the separator a literal and `n` a whole number.
fn split_index(e: &str) -> Option<&str> {
    let (call, index) = e.strip_suffix(']')?.rsplit_once('[')?;
    let digits = index.strip_prefix('-').unwrap_or(index);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (head, sep) = call.strip_suffix(')')?.rsplit_once(".split(")?;
    literal(sep).map(|_| head)
}

/// The variable passed to the `first_found` lookup when `name` is `{{ lookup('first_found', var) }}`
/// (or `query`, `q`) and nothing else.
fn first_found_lookup(name: &str) -> Option<&str> {
    let e = name.trim().strip_prefix("{{")?.strip_suffix("}}")?.trim();
    let args = ["lookup(", "query(", "q("]
        .iter()
        .find_map(|call| e.strip_prefix(call))?
        .strip_suffix(')')?;
    let (plugin, var) = args.split_once(',')?;
    let var = var.trim();
    let first_found = literal(plugin).map(short_name) == Some("first_found");
    (first_found && !var.is_empty() && var.bytes().all(is_word)).then_some(var)
}
