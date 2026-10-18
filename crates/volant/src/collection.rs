// SPDX-License-Identifier: GPL-3.0-or-later
//! The `collections` a task searches before `ansible.builtin`, and the `module_defaults` that
//! apply to the module it ends up calling.
//!
//! Measured on ansible-core 2.19.12: the layers of `module_defaults` are merged by key, so the
//! innermost layer naming a module replaces what an outer one said about it as a whole; the
//! names `x`, `ansible.builtin.x` and `ansible.legacy.x` are one key, the last one listed in a
//! layer winning; and a `group/<name>` key applies to every module of the group, under the
//! module's own key.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use serde_json::{Map, Value};

use crate::bridge::Bridge;
use crate::compile::{Compiled, Handler, Step};
use crate::playbook::PlayTask;
use crate::roles::RoleSearch;

/// `x` for `x`, `ansible.builtin.x` and `ansible.legacy.x`: the three names one key answers to.
pub(crate) fn canonical(name: &str) -> &str {
    name.strip_prefix("ansible.builtin.")
        .or_else(|| name.strip_prefix("ansible.legacy."))
        .unwrap_or(name)
}

/// A `namespace.name` or a module name made of the characters Python identifiers have: what is
/// joined into a path below, so a playbook cannot name a directory outside the collections.
fn is_word(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Where `collection` (`namespace.name`) is installed, searching the directories the playbook's
/// roles are searched with first.
fn home(search: &RoleSearch, collection: &str) -> Option<PathBuf> {
    let (namespace, name) = collection.split_once('.')?;
    if !is_word(namespace) || !is_word(name) {
        return None;
    }
    search
        .collections
        .iter()
        .map(|base| base.join("ansible_collections").join(namespace).join(name))
        .find(|dir| dir.is_dir())
}

/// The statements and the pseudo-module the engine answers itself: never looked up in a
/// collection.
fn is_statement(module: &str) -> bool {
    matches!(
        module,
        "meta"
            | "include_tasks"
            | "import_tasks"
            | "include_role"
            | "import_role"
            | "include_vars"
            | "import_playbook"
    )
}

/// Gives a task written with a short module name the name of the first listed collection that
/// has it. A name no listed collection has stays as written, for `ansible.builtin` to answer.
///
/// A listed collection nobody installed warns once per name looked for, in the reference's
/// words, and the search goes on; `warnings` takes each sentence once, the way the reference
/// prints it.
fn resolve(task: &mut PlayTask, search: &RoleSearch, warnings: &mut Vec<String>) {
    let Some(listed) = &task.collections else {
        return;
    };
    let module = task.module.as_str();
    if module.contains('.') || !is_word(module) || is_statement(module) {
        return;
    }
    let mut found = None;
    for collection in listed {
        let Some(dir) = home(search, collection) else {
            let (namespace, _) = collection.split_once('.').unwrap_or((collection, ""));
            let installed_namespace = search
                .collections
                .iter()
                .any(|base| base.join("ansible_collections").join(namespace).is_dir());
            let missing = if installed_namespace {
                collection.as_str()
            } else {
                namespace
            };
            let warning = format!(
                "Error loading plugin '{module}': No module named 'ansible_collections.{missing}'"
            );
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
            continue;
        };
        if ["modules", "action"].iter().any(|kind| {
            dir.join("plugins")
                .join(kind)
                .join(format!("{module}.py"))
                .is_file()
        }) {
            found = Some(format!("{collection}.{module}"));
            break;
        }
    }
    if let Some(full) = found {
        task.module = full;
    }
}

/// [`resolve`] for a task read from a file the run may include later, under the `collections`
/// of the statement or role that brings it in unless it lists its own. Nothing is said about a
/// collection nobody installed: the compilation of the play has said it already.
pub(crate) fn scope(tasks: &mut [PlayTask], inherited: Option<&[String]>, search: &RoleSearch) {
    for task in tasks {
        if task.collections.is_none() {
            task.collections = inherited.map(<[String]>::to_vec);
        }
        resolve(task, search, &mut Vec::new());
    }
}

/// [`resolve`] over every step and handler of a compiled play, with the warnings it raised.
pub(crate) fn resolve_all(
    steps: &mut [Step],
    handlers: &mut [Handler],
    search: &RoleSearch,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for task in steps
        .iter_mut()
        .map(|step| &mut step.task)
        .chain(handlers.iter_mut().map(|handler| &mut handler.task))
    {
        resolve(task, search, &mut warnings);
    }
    warnings
}

/// A group's full name: a bare one belongs to `ansible.builtin`, as the reference reads it, and a
/// name of three parts or more is already one.
pub(crate) fn group_fqcn(name: &str) -> String {
    if name.split('.').count() >= 3 {
        name.to_string()
    } else {
        format!("ansible.builtin.{name}")
    }
}

/// The groups every task and handler of `steps` names in its `module_defaults`, by full name.
pub(crate) fn groups_named<'a>(tasks: impl IntoIterator<Item = &'a PlayTask>) -> BTreeSet<String> {
    tasks
        .into_iter()
        .flat_map(|task| &task.module_defaults)
        .filter_map(Value::as_object)
        .flat_map(Map::keys)
        .filter_map(|key| key.strip_prefix("group/"))
        .map(group_fqcn)
        .collect()
}

/// Every task and handler of a compiled play.
pub(crate) fn tasks_of(play: &Compiled) -> impl Iterator<Item = &PlayTask> {
    play.steps
        .iter()
        .map(|step| &step.task)
        .chain(play.handlers.iter().map(|handler| &handler.task))
}

/// The names the reference resolves for the plain keys of `module_defaults`, a short key under
/// `ansible.legacy.`. A builtin module needs no asking.
pub(crate) fn actions_named<'a>(tasks: impl IntoIterator<Item = &'a PlayTask>) -> BTreeSet<String> {
    tasks
        .into_iter()
        .flat_map(|task| &task.module_defaults)
        .filter_map(Value::as_object)
        .flat_map(Map::keys)
        .filter(|key| !key.starts_with("group/") && !volant_protocol::modules::is_builtin(key))
        .map(|key| {
            if key.split('.').count() < 3 {
                format!("ansible.legacy.{key}")
            } else {
                key.clone()
            }
        })
        .collect()
}

/// The sentence for a key that names nothing.
pub(crate) fn unresolved_action(name: &str) -> String {
    format!("Could not resolve action {name} in module_defaults")
}

/// What ansible-core said about the groups and the keys of `module_defaults`, shared by every
/// play of a run. A name an include brings in while the play runs is asked about then.
#[derive(Debug, Default)]
pub(crate) struct Resolved {
    groups: Mutex<BTreeMap<String, Vec<String>>>,
    actions: Mutex<BTreeSet<String>>,
}

impl Resolved {
    /// Asks `bridge` about every group and key the layers of `tasks` name that has not been
    /// asked about yet.
    ///
    /// # Errors
    /// The reference's sentence for the first group or key nothing answers to.
    pub(crate) fn ensure<'a>(
        &self,
        tasks: impl IntoIterator<Item = &'a PlayTask> + Clone,
        bridge: Option<&Bridge>,
    ) -> Result<(), String> {
        let missing_actions: Vec<String> = {
            let known = self.actions.lock().unwrap_or_else(PoisonError::into_inner);
            actions_named(tasks.clone())
                .into_iter()
                .filter(|name| !known.contains(name))
                .collect()
        };
        if let Some(first) = missing_actions.first() {
            let bridge = bridge.ok_or_else(|| unresolved_action(first))?;
            let found = bridge
                .actions_resolve(&missing_actions)
                .map_err(|e| e.to_string())?;
            if let Some((name, _)) = found.iter().find(|(_, resolved)| !**resolved) {
                return Err(unresolved_action(name));
            }
            self.actions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(missing_actions);
        }
        let missing_groups: Vec<String> = {
            let known = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
            groups_named(tasks)
                .into_iter()
                .filter(|name| !known.contains_key(name))
                .collect()
        };
        if let Some(first) = missing_groups.first() {
            let bridge = bridge.ok_or_else(|| unresolved_group(first))?;
            let held = bridge
                .action_groups(&missing_groups)
                .map_err(|e| e.to_string())?;
            let mut known = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
            for (group, members) in held {
                known.insert(
                    group.clone(),
                    members.ok_or_else(|| unresolved_group(&group))?,
                );
            }
        }
        Ok(())
    }

    /// The groups resolved so far, by full name.
    pub(crate) fn groups(&self) -> BTreeMap<String, Vec<String>> {
        self.groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The sentence for a group nothing defines.
pub(crate) fn unresolved_group(group: &str) -> String {
    format!("Error loading module_defaults: could not resolve the module_defaults group {group}")
}

/// One `module_defaults` value as the mapping it stands for, or why it is none.
pub(crate) type Rendered = Result<Map<String, Value>, String>;

/// What `module_defaults` gives the call to `module`, before the task's own arguments are laid
/// over it. `groups` maps a group's full name to the modules it holds; `render` turns the value
/// a key holds into the mapping it stands for.
///
/// # Errors
/// The reference's sentence for a group `groups` does not hold, or whatever `render` refuses.
pub(crate) fn defaults_for(
    module: &str,
    layers: &[Value],
    groups: &BTreeMap<String, Vec<String>>,
    render: &mut dyn FnMut(&str, &Value) -> Rendered,
) -> Result<Map<String, Value>, String> {
    let mut merged: Vec<(String, &Value)> = Vec::new();
    for layer in layers {
        let Value::Object(layer) = layer else {
            continue;
        };
        for (key, value) in layer {
            let key = match key.strip_prefix("group/") {
                Some(group) => format!("group/{}", group_fqcn(group)),
                None => canonical(key).to_string(),
            };
            match merged.iter_mut().find(|(seen, _)| *seen == key) {
                Some(slot) => slot.1 = value,
                None => merged.push((key, value)),
            }
        }
    }
    let me = canonical(module);
    let mut out = Map::new();
    // Each value is rendered by the caller before it is required to be a mapping: a whole value
    // may be a template that renders to one.
    let mut lay = |key: &str, value: &Value| {
        out.extend(render(key, value)?);
        Ok::<(), String>(())
    };
    for (key, value) in &merged {
        if let Some(group) = key.strip_prefix("group/") {
            let members = groups.get(group).ok_or_else(|| unresolved_group(group))?;
            if members.iter().any(|member| canonical(member) == me) {
                lay(key, value)?;
            }
        }
    }
    if let Some((key, value)) = merged.iter().find(|(key, _)| key == me) {
        lay(key, value)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
