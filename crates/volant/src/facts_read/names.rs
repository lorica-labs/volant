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

    /// The file name terms of a `with_first_found` loop, as patterns.
    pub(super) fn first_found_terms(&self, task: &PlayTask) -> Option<Vec<String>> {
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
