// SPDX-License-Identifier: GPL-3.0-or-later
//! The walk's state and the places it reads from: variable definitions, files and the pending
//! templated names.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::decide::{ANY, MOST_PASSES, OTHER_MODULES_KEYS};

use crate::compile::Origin;
use crate::executor::strings;
use crate::playbook::PlayTask;

use crate::template::Templar;

pub(super) struct Scan {
    pub(super) base: PathBuf,
    pub(super) keys: BTreeSet<String>,
    /// The first construct found that can read any fact, with where it was found.
    pub(super) all: Option<String>,
    /// Every value each variable is given anywhere the scan reads.
    pub(super) defs: BTreeMap<String, Vec<Value>>,
    /// Whether some variable is named by a template (`set_fact` with a templated key), so that
    /// no variable's values are all known.
    pub(super) defs_unknown: bool,
    /// The files named by a template, resolved once every value is known.
    pub(super) pending: Vec<Pending>,
    /// Every task file the walk read.
    pub(super) task_files: BTreeSet<PathBuf>,
}

/// What a templated file name is looked for as.
#[derive(Clone, Copy)]
pub(super) enum Wanted {
    Template,
    Vars,
    Tasks,
}

/// A task whose file name is a template: `template` `src`, `include_vars`, `include_tasks`.
pub(super) struct Pending {
    pub(super) wanted: Wanted,
    pub(super) name: String,
    pub(super) task: PlayTask,
    pub(super) origin: Origin,
    pub(super) place: String,
}

impl Scan {
    pub(super) fn dynamic(&mut self, what: &str, place: &str) {
        if self.all.is_none() {
            self.all = Some(format!("{what} in {place}"));
        }
    }

    pub(super) fn key(&mut self, key: &str) {
        let key = key.split('.').next().unwrap_or(key);
        if !OTHER_MODULES_KEYS.contains(&key) {
            self.keys.insert(key.to_string());
        }
    }

    /// `path` relative to the playbook's directory when it is under it.
    pub(super) fn shown(&self, path: &Path) -> String {
        path.strip_prefix(&self.base)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    pub(super) fn map(&mut self, map: &Map<String, Value>, place: &str) {
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
    pub(super) fn file(&mut self, path: &Path) {
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
    pub(super) fn define(&mut self, name: &str, value: &Value) {
        if Templar::is_template(name) {
            self.defs_unknown = true;
            return;
        }
        let values = self.defs.entry(name.to_string()).or_default();
        if !values.contains(value) {
            values.push(value.clone());
        }
    }

    pub(super) fn define_all(&mut self, value: &Value) {
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
    pub(super) fn find(&mut self, origin: &Origin, sub: &str, name: &str) -> bool {
        let found = crate::action_plugins::files::search_paths(origin, &self.base, sub, name)
            .into_iter()
            .find(|p| p.is_file());
        found.map(|path| self.file(&path)).is_some()
    }

    pub(super) fn later(
        &mut self,
        wanted: Wanted,
        name: &str,
        task: &PlayTask,
        origin: &Origin,
        place: &str,
    ) {
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
    pub(super) fn settle(&mut self) {
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
}

/// Every file matching `pattern`, where [`ANY`] in a component stands for any run of characters.
pub(super) fn glob(pattern: &Path) -> Vec<PathBuf> {
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
pub(super) fn wild_match(pattern: &str, name: &str) -> bool {
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
