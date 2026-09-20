// SPDX-License-Identifier: GPL-3.0-or-later
//! Builtin modules the reference runs through an action plugin rather than by shipping the
//! module. Measured on ansible-core 2.19.12 from `action_loader`: 28 action plugins, 72 builtin
//! modules, 27 names in both.
//!
//! A module here cannot be run by sending its payload to the agent: the action plugin is where
//! its real behaviour lives. `package` picks the host's package manager, `service` picks its
//! init system, `template` renders on the controller. Sending the module alone would run
//! something that is not what the playbook asked for.
//!
//! The names this release already implements are **not** here, whether natively (`command`,
//! `shell`, `raw`) or on the controller (`debug`, `set_fact`, `include_vars`,
//! `validate_argument_spec`). `normal` is not here either: it is the only action plugin with no
//! module of the same name, so no playbook can name it.
pub const BUILTIN_ACTION_PLUGINS: &[&str] = &[
    "add_host",
    "assemble",
    "assert",
    "async_status",
    "copy",
    "dnf",
    "fail",
    "fetch",
    "gather_facts",
    "group_by",
    "package",
    "pause",
    "reboot",
    "script",
    "service",
    "set_stats",
    "template",
    "unarchive",
    "uri",
    "wait_for_connection",
];

/// Whether a playbook's module name is one of those.
///
/// Read the same way the builtin registry reads one: `ansible.builtin.` and `ansible.legacy.`
/// name the same modules as the bare name, and any other collection is somebody else's module.
pub fn is_action_backed(module: &str) -> bool {
    let ours = !module.contains('.')
        || module.starts_with("ansible.builtin.")
        || module.starts_with("ansible.legacy.");
    ours && BUILTIN_ACTION_PLUGINS.contains(&volant_protocol::modules::short_name(module))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list holds what the reference runs through an action plugin and nothing this
    /// release already runs itself.
    ///
    /// What would make this red: `setup` slipping in - it is `gather_facts` that has the action
    /// plugin, and a run that cannot call `setup` gathers no facts at all - or one of the four
    /// controller-side modules, which would refuse a playbook this release runs today.
    #[test]
    fn the_list_names_only_what_this_release_cannot_run() {
        let mut sorted = BUILTIN_ACTION_PLUGINS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, BUILTIN_ACTION_PLUGINS, "the list is kept sorted");
        assert_eq!(BUILTIN_ACTION_PLUGINS.len(), 20);
        for absent in [
            "setup",
            "command",
            "shell",
            "raw",
            "debug",
            "set_fact",
            "include_vars",
        ] {
            assert!(!is_action_backed(absent), "{absent} is not action-backed");
        }
        for present in ["package", "service", "template"] {
            assert!(is_action_backed(present), "{present} is action-backed");
        }
    }

    /// The two prefixes that name the same modules are read as such, and another collection's
    /// module of the same name is not this one.
    #[test]
    fn the_builtin_prefixes_name_the_same_modules() {
        assert!(is_action_backed("ansible.builtin.template"));
        assert!(is_action_backed("ansible.legacy.template"));
        assert!(!is_action_backed("community.general.template"));
    }
}
