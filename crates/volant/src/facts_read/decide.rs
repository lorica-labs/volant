// SPDX-License-Identifier: GPL-3.0-or-later
//! The public answer: which facts a run reads, and whether the native `setup` covers them.

use std::collections::{BTreeMap, BTreeSet};

use volant_protocol::facts::NATIVE_FACT_KEYS;

use super::FactsRead;

use super::scan::Scan;
use crate::compile::{Compiled, Origin};

use crate::playbook::Play;
use crate::python::{Facts, Reach};
use crate::template::Templar;
use crate::vars::VarStore;

/// `ansible_<name>` variables that are not facts: the magic variables and the connection
/// variables of ansible-core 2.19.12. Reading one reads no fact.
pub(super) const NOT_FACTS: &[&str] = &[
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
pub(super) const NOT_FACT_PREFIXES: &[&str] = &[
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
pub(super) const OTHER_MODULES_KEYS: &[&str] =
    &["aggregate_mounts", "mount_points", "packages", "services"];

/// The lookups whose argument names a variable or a template file this scan does not open.
pub(super) const DYNAMIC_LOOKUPS: &[&str] = &["vars", "template"];

/// Facts whose value is one name and never a path: a file name built from one stays a name in the
/// directory it is looked for in (`include_vars: "{{ ansible_facts.os_family }}.yml"`).
pub(super) const NAME_FACTS: &[&str] = &[
    "architecture",
    "distribution",
    "distribution_major_version",
    "distribution_release",
    "distribution_version",
    "machine",
    "os_family",
    "pkg_mgr",
    "service_mgr",
    "system",
];

/// The most file names one templated name may stand for before the scan stops counting them.
pub(super) const MOST_NAMES: usize = 256;

/// In a file name pattern, any run of characters but `/`: what a [`NAME_FACTS`] value renders to.
pub(super) const ANY: char = '\0';

/// How many times the templated names are resolved again because a file one of them named
/// defined a variable another reads.
pub(super) const MOST_PASSES: usize = 8;

/// Every fact name the run's plays can read: in the text of every task and handler they reach,
/// every template and vars file in the roles they reach, the body of a template with a literal
/// `src`, a literal `include_vars` file and `vars_files` entry, role parameters, play `vars:`,
/// the inventory and its `group_vars` and `host_vars`, and the command line's variables.
///
/// A role is read whole, used or not: every file under its `templates/`, `vars/`, `defaults/`
/// and `meta/`, and every task file. A templated `src`, `include_vars` or `include_tasks` name
/// counts as read only when every file it can render to is known and read: each variable in it
/// given plain string values alone wherever the scan looks, each loop item from a literal list,
/// each fact one whose value is a single name ([`NAME_FACTS`]), and for `include_tasks` every
/// matching file one the walk read. Anything else reads every fact.
pub(crate) fn facts_read(
    plays: &[(&Play, &Compiled)],
    reach: &Reach,
    store: &VarStore,
) -> FactsRead {
    let mut scan = Scan {
        base: store.playbook_dir().to_path_buf(),
        keys: BTreeSet::new(),
        all: None,
        defs: BTreeMap::new(),
        defs_unknown: false,
        pending: Vec::new(),
        task_files: reach.files.clone(),
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
    scan.settle();
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

pub(super) fn not_fact(key: &str) -> bool {
    NOT_FACTS.contains(&key)
        || NOT_FACT_PREFIXES.iter().any(|p| key.starts_with(p))
        || key.ends_with("_interpreter")
        || (key.starts_with("ssh_") && !key.starts_with("ssh_host_key_"))
}
