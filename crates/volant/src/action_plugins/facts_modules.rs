// SPDX-License-Identifier: GPL-3.0-or-later
//! The fact modules `gather_facts` may run besides `setup`: what `ansible_facts_modules` and
//! `FACTS_MODULES` name. The run builds every module it may run before the first connection, so
//! the names are read where a literal list writes them: the plays' `vars` and tasks, their roles'
//! task files, the inventory and the configuration. A name only a template gives is not known in
//! time.

use std::sync::OnceLock;

use serde_json::Value;

use crate::config::Config;
use crate::inventory::Inventory;
use crate::playbook::{Play, TaskOrBlock};
use crate::roles::RoleSearch;

/// The names [`configure`] found, as a sub-task of the plugin names them.
static EXTRA: OnceLock<&'static [&'static str]> = OnceLock::new();

/// The variable a play or a host names its fact modules with.
const VARIABLE: &str = "ansible_facts_modules";

/// Reads the fact modules the run names, once, before anything asks [`modules`].
pub(crate) fn configure<'a>(
    config: &Config,
    plays: impl IntoIterator<Item = &'a Play>,
    inventory: &Inventory,
) {
    let mut names: Vec<String> = config.facts_modules.clone();
    for play in plays {
        names.extend(named(|key| play.vars.get(key)));
        let tasks = play
            .pre_tasks
            .iter()
            .chain(&play.tasks)
            .chain(&play.post_tasks);
        tasks_naming(tasks, &mut names);
        // A role of the play's `roles:` is read at compile time, after this: its task files are
        // read here for the names they write.
        let search = RoleSearch::new(&play.dir, config);
        for role in &play.roles {
            let Ok(dir) = search.locate(&role.name) else {
                continue;
            };
            let Ok(files) = std::fs::read_dir(dir.join("tasks")) else {
                continue;
            };
            for path in files.flatten().map(|f| f.path()) {
                if let Ok(items) = crate::playbook::parse_tasks_file(&path) {
                    tasks_naming(&items, &mut names);
                }
            }
        }
    }
    for host in inventory.all_host_names() {
        let host = inventory.host_with_vars(&host);
        names.extend(named(|key| host.vars.get(key)));
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

/// The names the tasks' and blocks' `vars` write, at any depth.
fn tasks_naming<'a>(items: impl IntoIterator<Item = &'a TaskOrBlock>, names: &mut Vec<String>) {
    let mut open: Vec<&TaskOrBlock> = items.into_iter().collect();
    while let Some(entry) = open.pop() {
        match entry {
            TaskOrBlock::Task(task) => names.extend(named(|key| task.vars.get(key))),
            TaskOrBlock::Block(block) => {
                names.extend(named(|key| block.keywords.vars.get(key)));
                open.extend(block.body.iter().chain(&block.rescue).chain(&block.always));
            }
        }
    }
}

/// The fact modules one set of variables names: its `ansible_facts_modules`, and the module
/// `CONNECTION_FACTS_MODULES` gives its `ansible_network_os`, which `gather_facts` adds.
fn named<'v>(get: impl Fn(&str) -> Option<&'v Value>) -> Vec<String> {
    let mut names = literal(get(VARIABLE));
    for os in literal(get("ansible_network_os")) {
        let found = CONNECTION_FACTS_MODULES
            .iter()
            .find(|(name, _)| *name == os);
        names.extend(found.map(|(_, module)| (*module).to_string()));
    }
    names
}

/// ansible-core 2.19.12's default `CONNECTION_FACTS_MODULES`: the fact module of each network OS.
const CONNECTION_FACTS_MODULES: [(&str, &str); 24] = [
    ("asa", "ansible.legacy.asa_facts"),
    ("cisco.asa.asa", "cisco.asa.asa_facts"),
    ("eos", "ansible.legacy.eos_facts"),
    ("arista.eos.eos", "arista.eos.eos_facts"),
    ("frr", "ansible.legacy.frr_facts"),
    ("frr.frr.frr", "frr.frr.frr_facts"),
    ("ios", "ansible.legacy.ios_facts"),
    ("cisco.ios.ios", "cisco.ios.ios_facts"),
    ("iosxr", "ansible.legacy.iosxr_facts"),
    ("cisco.iosxr.iosxr", "cisco.iosxr.iosxr_facts"),
    ("junos", "ansible.legacy.junos_facts"),
    (
        "junipernetworks.junos.junos",
        "junipernetworks.junos.junos_facts",
    ),
    ("nxos", "ansible.legacy.nxos_facts"),
    ("cisco.nxos.nxos", "cisco.nxos.nxos_facts"),
    ("vyos", "ansible.legacy.vyos_facts"),
    ("vyos.vyos.vyos", "vyos.vyos.vyos_facts"),
    ("exos", "ansible.legacy.exos_facts"),
    ("extreme.exos.exos", "extreme.exos.exos_facts"),
    ("slxos", "ansible.legacy.slxos_facts"),
    ("extreme.slxos.slxos", "extreme.slxos.slxos_facts"),
    ("voss", "ansible.legacy.voss_facts"),
    ("extreme.voss.voss", "extreme.voss.voss_facts"),
    ("ironware", "ansible.legacy.ironware_facts"),
    (
        "community.network.ironware",
        "community.network.ironware_facts",
    ),
];

/// The names a literal value of a variable holds: a list of strings or one string.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A network OS names the fact module `gather_facts` adds for it; one the table lacks, none.
    #[test]
    fn a_network_os_names_its_fact_module() {
        let vars = serde_json::json!({
            "ansible_facts_modules": ["mine"],
            "ansible_network_os": "cisco.ios.ios",
        });
        assert_eq!(named(|key| vars.get(key)), ["mine", "cisco.ios.ios_facts"]);
        let vars = serde_json::json!({"ansible_network_os": ["ios", "nosuch"]});
        assert_eq!(named(|key| vars.get(key)), ["ansible.legacy.ios_facts"]);
    }
}
