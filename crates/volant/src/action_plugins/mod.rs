// SPDX-License-Identifier: GPL-3.0-or-later
//! Builtin modules the reference runs through an action plugin rather than by shipping the
//! module, and the plugins this release runs itself.
//!
//! Measured on ansible-core 2.19.12 from `action_loader`: 28 action plugins, 72 builtin modules,
//! 27 names in both. A module whose plugin is not written here cannot be run by sending its
//! payload to the agent: the action plugin is where its real behaviour lives. `unarchive` reads
//! its archive on the controller, `fetch` writes there. Sending the module alone would run
//! something that is not what the playbook asked for, so those names are refused before the first
//! connection.
//!
//! A plugin that is written here runs on the controller as a small state machine: asked for the
//! next sub-task, handed the result of the last one, until it hands back the task's result. Each
//! sub-task is an ordinary Python module of the run's union, sent alone over the link the task
//! already has. The result the plugin ends with goes down the one road every remote result takes,
//! so no host value reaches the variables by a way of its own.

pub(crate) mod copy;
pub(crate) mod files;
mod package;
mod service;
mod template;

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use crate::template::Templar;
use crate::vars::HostVars;

/// The action plugins of the reference this release still refuses.
///
/// The names this release already implements are **not** here, whether natively (`command`,
/// `shell`, `raw`), on the controller (`assert`, `debug`, `fail`, `include_vars`, `pause`,
/// `set_fact`, `validate_argument_spec`) or through a plugin of its own (`copy`, `package`,
/// `service`, `template`). `normal` is not here either: it is the only action plugin with no
/// module of the same name, so no playbook can name it.
pub const BUILTIN_ACTION_PLUGINS: &[&str] = &[
    "add_host",
    "assemble",
    "async_status",
    "dnf",
    "fetch",
    "gather_facts",
    "group_by",
    "reboot",
    "script",
    "set_stats",
    "unarchive",
    "uri",
    "wait_for_connection",
];

/// Whether a module name is spelled as one of the builtin modules: bare, or under one of the two
/// prefixes that name the same modules. Any other collection is somebody else's module.
fn builtin_spelling(module: &str) -> bool {
    !module.contains('.')
        || module.starts_with("ansible.builtin.")
        || module.starts_with("ansible.legacy.")
}

/// Whether a playbook's module name is one of [`BUILTIN_ACTION_PLUGINS`].
///
/// Read the same way the builtin registry reads one: `ansible.builtin.` and `ansible.legacy.`
/// name the same modules as the bare name, and any other collection is somebody else's module.
pub fn is_action_backed(module: &str) -> bool {
    builtin_spelling(module) && BUILTIN_ACTION_PLUGINS.contains(&short_name(module))
}

/// An action plugin this release runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Copy,
    Package,
    Service,
    Template,
}

/// The plugin this release runs for a module name, read as the builtin registry reads one.
pub(crate) fn kind(module: &str) -> Option<Kind> {
    if !builtin_spelling(module) {
        return None;
    }
    match short_name(module) {
        "copy" => Some(Kind::Copy),
        "package" => Some(Kind::Package),
        "service" => Some(Kind::Service),
        "template" => Some(Kind::Template),
        _ => None,
    }
}

/// Every module a plugin may run, which the union has to hold before the first connection.
///
/// A run that names `package` or `service` carries every builtin backend and `setup`. Measured
/// with ansible-core 2.19.12's own payload builder, that is 170 KB more than `apt` and
/// `systemd_service` alone, sent once per link. Nothing is built after the
/// facts are known, so a plugin can only ever pick among these.
pub(crate) fn modules_for(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Copy | Kind::Template => &["stat", "file", "copy"],
        Kind::Package => &["setup", "apt", "dnf", "dnf5"],
        Kind::Service => &["setup", "systemd", "systemd_service", "sysvinit", "service"],
    }
}

/// A file a sub-task needs on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileBlob {
    /// Lowercase hex blake3 of the bytes, which names the blob the way the union's hash does.
    pub hash: String,
    pub b64: String,
}

/// One module run on the host, on the plugin's behalf.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sub {
    /// Short builtin name, which the union is keyed by.
    pub module: &'static str,
    pub args: Map<String, Value>,
    /// `(argument, blob)`: staged by the agent under that argument.
    pub files: Vec<(String, FileBlob)>,
}

impl Sub {
    /// A sub-task that stages nothing.
    fn run(module: &'static str, args: Map<String, Value>) -> Step {
        Step::Run(Sub {
            module,
            args,
            files: Vec::new(),
        })
    }
}

/// What a plugin asks for next.
pub(crate) enum Step {
    /// One more module on the host; its result comes back on the next call.
    Run(Sub),
    /// The task's result for this item.
    Done(TaskResult),
}

/// A plugin, item by item: asked for the next sub-task, handed the result of the last.
pub(crate) trait Plugin: Send {
    fn next(&mut self, last: Option<TaskResult>) -> Step;
}

/// What a plugin reads of the task and the host, all of it already rendered.
pub(crate) struct Context<'a> {
    /// The item's rendered arguments and the names among them whose render read a host.
    pub args: &'a Map<String, Value>,
    pub args_untrusted: &'a BTreeSet<String>,
    /// The variables of the host the module runs on - the delegate's when there is one.
    pub running_vars: &'a Map<String, Value>,
    /// The item's own variables, for `template`.
    pub item_vars: &'a HostVars,
    pub templar: &'a Templar,
    /// Where the task was written, for `src` search paths.
    pub origin: &'a crate::compile::Origin,
    pub playbook_dir: &'a Path,
    /// Lines to show as `[WARNING]:` under this task.
    pub warnings: &'a mut Vec<String>,
}

pub(crate) fn start(kind: Kind, ctx: Context<'_>) -> Box<dyn Plugin + '_> {
    match kind {
        Kind::Copy => copy::start(ctx),
        Kind::Package => Box::new(package::Package::new(ctx)),
        Kind::Service => Box::new(service::Service::new(ctx)),
        Kind::Template => template::start(ctx),
    }
}

/// The filtered `setup` both plugins run when the facts do not say which backend to use, exactly
/// as ansible-core 2.19.12 was measured to send it: one fact, and no subset gathered.
fn setup_for(fact: &str) -> Step {
    let mut args = Map::new();
    args.insert("filter".into(), Value::from(vec![fact]));
    args.insert("gather_subset".into(), Value::from(vec!["!all"]));
    Sub::run("setup", args)
}

/// `ansible_facts.<name>` of the host the module runs on, when it is a string.
fn fact<'a>(vars: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    vars.get("ansible_facts")?.get(name)?.as_str()
}

/// The one fact a filtered `setup` was asked for, off its result.
fn gathered<'a>(result: &'a TaskResult, name: &str) -> Option<&'a str> {
    result.0.get("ansible_facts")?.get(name)?.as_str()
}

/// The result a sub-task is expected to have handed back and did not. The driver hands every
/// sub-task's result to the next call, so this is a controller bug that fails the item by name
/// rather than a panic that takes the host's driver down.
fn lost(module: &str) -> TaskResult {
    TaskResult::failed_with(format!("the result of the '{module}' sub-task was lost"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list holds what the reference runs through an action plugin and nothing this
    /// release already runs itself.
    ///
    /// What would make this red: `setup` slipping in - it is `gather_facts` that has the action
    /// plugin, and a run that cannot call `setup` gathers no facts at all - or one of the four
    /// controller-side modules, which would refuse a playbook this release runs today; or a
    /// plugin this release runs left in, which refuses it before its dispatch is ever reached.
    #[test]
    fn the_list_names_only_what_this_release_cannot_run() {
        let mut sorted = BUILTIN_ACTION_PLUGINS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, BUILTIN_ACTION_PLUGINS, "the list is kept sorted");
        assert_eq!(BUILTIN_ACTION_PLUGINS.len(), 13);
        for absent in [
            "setup",
            "command",
            "shell",
            "raw",
            "debug",
            "set_fact",
            "include_vars",
            "assert",
            "fail",
            "pause",
            "package",
            "service",
            "copy",
            "template",
        ] {
            assert!(!is_action_backed(absent), "{absent} is not action-backed");
        }
        for present in ["unarchive", "fetch"] {
            assert!(is_action_backed(present), "{present} is action-backed");
        }
    }

    /// The two prefixes that name the same modules are read as such, and another collection's
    /// module of the same name is not this one.
    #[test]
    fn the_builtin_prefixes_name_the_same_modules() {
        assert!(is_action_backed("ansible.builtin.unarchive"));
        assert!(is_action_backed("ansible.legacy.unarchive"));
        assert!(!is_action_backed("community.general.unarchive"));
        assert_eq!(kind("ansible.builtin.package"), Some(Kind::Package));
        assert_eq!(kind("ansible.legacy.service"), Some(Kind::Service));
        assert_eq!(kind("community.general.package"), None);
        assert_eq!(kind("ansible.builtin.copy"), Some(Kind::Copy));
        assert_eq!(kind("ansible.builtin.template"), Some(Kind::Template));
        assert_eq!(kind("unarchive"), None);
    }

    /// Every module a plugin can run is a builtin module ansible-core builds a payload for, and
    /// none is one this engine runs natively or on the controller.
    ///
    /// What would make this red: a name the union helper cannot build, which refuses every run
    /// naming the plugin before the first connection, or a native one, which would go out as a
    /// payload and stop running natively. `dnf` and `service` are plugins' names too and belong
    /// here all the same: a plugin runs the module directly, as the reference's does.
    #[test]
    fn a_plugin_runs_only_what_the_union_can_hold() {
        for kind in [Kind::Copy, Kind::Package, Kind::Service, Kind::Template] {
            for module in modules_for(kind) {
                assert!(volant_protocol::modules::is_builtin(module), "{module}");
                assert!(!volant_protocol::modules::is_known(module), "{module}");
            }
        }
    }
}
