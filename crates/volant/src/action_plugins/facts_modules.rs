// SPDX-License-Identifier: GPL-3.0-or-later
//! The fact modules `gather_facts` may run besides `setup`: what `ansible_facts_modules` and
//! `FACTS_MODULES` name. The run builds every module it may run before the first connection, so
//! the names are read where a literal list writes them: the plays' `vars`, the inventory and the
//! configuration. A name only a template gives is not known in time.

use std::sync::OnceLock;

use serde_json::Value;

use crate::inventory::Inventory;
use crate::playbook::{Play, TaskOrBlock};

/// The names [`configure`] found, as a sub-task of the plugin names them.
static EXTRA: OnceLock<&'static [&'static str]> = OnceLock::new();

/// The variable a play or a host names its fact modules with.
const VARIABLE: &str = "ansible_facts_modules";

/// Reads the fact modules the run names, once, before anything asks [`modules`].
pub(crate) fn configure<'a>(
    configured: &[String],
    plays: impl IntoIterator<Item = &'a Play>,
    inventory: &Inventory,
) {
    let mut names: Vec<String> = configured.to_vec();
    for play in plays {
        names.extend(literal(play.vars.get(VARIABLE)));
        let mut open: Vec<&TaskOrBlock> = play
            .pre_tasks
            .iter()
            .chain(&play.tasks)
            .chain(&play.post_tasks)
            .collect();
        while let Some(entry) = open.pop() {
            match entry {
                TaskOrBlock::Task(task) => names.extend(literal(task.vars.get(VARIABLE))),
                TaskOrBlock::Block(block) => {
                    names.extend(literal(block.keywords.vars.get(VARIABLE)));
                    open.extend(block.body.iter().chain(&block.rescue).chain(&block.always));
                }
            }
        }
    }
    for host in inventory.all_host_names() {
        names.extend(literal(inventory.host_with_vars(&host).vars.get(VARIABLE)));
    }
    let mut out: Vec<&'static str> = Vec::new();
    for name in names {
        let short = match name
            .strip_prefix("ansible.builtin.")
            .or(name.strip_prefix("ansible.legacy."))
        {
            Some(short) => short.to_string(),
            None => name,
        };
        // `smart` picks `setup` on the hosts this release reaches.
        if matches!(short.as_str(), "smart" | "setup") || out.contains(&short.as_str()) {
            continue;
        }
        out.push(Box::leak(short.into_boxed_str()));
    }
    let _ = EXTRA.set(Box::leak(out.into_boxed_slice()));
}

/// The fact modules besides `setup` the run names, empty when it names none.
pub(crate) fn extra() -> &'static [&'static str] {
    EXTRA.get().copied().unwrap_or_default()
}

/// The names of [`extra`] the run could not build a module for.
static MISSING: OnceLock<Vec<&'static str>> = OnceLock::new();

/// Notes which of [`extra`] the union holds no module for, once it is built.
pub(crate) fn settle(union: Option<&crate::python::Union>) {
    let missing = extra()
        .iter()
        .copied()
        .filter(|name| union.is_none_or(|u| u.payload(name).is_none()))
        .collect();
    let _ = MISSING.set(missing);
}

/// The reference's failure for a fact module `name` the run could not build, when it is one:
/// measured on ansible-core 2.19.12, `gather_facts` fails with it.
pub(crate) fn missing(name: &str) -> Option<String> {
    MISSING
        .get()?
        .contains(&name)
        .then(|| format!("The module {name} was not found in configured module paths"))
}

/// Every module `gather_facts` may run: `setup` and [`extra`].
pub(crate) fn modules() -> &'static [&'static str] {
    static ALL: OnceLock<&'static [&'static str]> = OnceLock::new();
    if extra().is_empty() {
        return &["setup"];
    }
    ALL.get_or_init(|| {
        let mut all = vec!["setup"];
        all.extend(extra());
        Box::leak(all.into_boxed_slice())
    })
}

/// The names a literal value of the variable holds: a list of strings or one string.
fn literal(value: Option<&Value>) -> Vec<String> {
    let text = |v: &Value| {
        v.as_str()
            .filter(|s| !crate::template::Templar::is_template(s))
            .map(str::to_string)
    };
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(text).collect(),
        Some(one @ Value::String(_)) => text(one).into_iter().collect(),
        _ => Vec::new(),
    }
}
