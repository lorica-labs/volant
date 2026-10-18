// SPDX-License-Identifier: GPL-3.0-or-later

//! The builtin modules the reference runs through an action plugin.

/// The action plugins this release runs on the controller, with what each does.
///
/// Measured on ansible-core 2.19.12 from `action_loader`: 28 action plugins, 72 builtin modules,
/// 27 names in both. The ones not here are run natively (`async_status`, `command`, `shell`,
/// `raw`) or on the controller (`assert`, `debug`, `fail`, `include_vars`, `pause`, `set_fact`,
/// `validate_argument_spec`). `normal` has no module of its own name, so no playbook names it.
pub const ACTION_PLUGINS: &[(&str, &str)] = &[
    (
        "add_host",
        "Add a host, or groups to a host, in the running play's inventory.",
    ),
    (
        "assemble",
        "Join the fragments of a directory into one file on the host.",
    ),
    (
        "copy",
        "Copy a file from the controller, or `content:`, to the host.",
    ),
    (
        "dnf",
        "Install or remove packages with `dnf` or `dnf5`, whichever the host runs.",
    ),
    (
        "fetch",
        "Copy a file from the host to the controller, under `dest` and nowhere else.",
    ),
    ("gather_facts", "Gather the host's facts as a task."),
    (
        "group_by",
        "Put the host in a group named from its variables.",
    ),
    (
        "package",
        "Install or remove packages with the host's own package manager.",
    ),
    (
        "reboot",
        "Reboot the host and wait until it has booted again.",
    ),
    (
        "script",
        "Copy a script from the controller to the host and run it there.",
    ),
    (
        "service",
        "Manage a service with the host's own init system.",
    ),
    ("set_stats", "Record custom statistics for the recap."),
    (
        "template",
        "Render a template on the controller and copy the result to the host.",
    ),
    (
        "unarchive",
        "Extract an archive read on the controller, or one already on the host.",
    ),
    ("uri", "Send an HTTP request from the host."),
    ("wait_for_connection", "Wait until the host answers again."),
];
