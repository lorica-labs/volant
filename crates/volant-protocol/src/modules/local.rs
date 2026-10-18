// SPDX-License-Identifier: GPL-3.0-or-later

//! The modules the controller runs itself, and the statements the compiler answers.

use super::{ModuleSpec, honoured, short_name};

/// Measured on ansible-core 2.19.12. `msg` is the alias the reference gives `fail_msg`, listed
/// here as a name of its own so that a task writing it is not refused. `quiet` changes only how
/// the reference prints the result, never the result itself, and the driver prints it that way.
pub const ASSERT: ModuleSpec = ModuleSpec {
    name: "assert",
    free_form: false,
    summary: "Fail the task unless every condition in `that` holds.",
    args: &[
        honoured("fail_msg"),
        honoured("msg"),
        honoured("quiet"),
        honoured("success_msg"),
        honoured("that"),
    ],
    validated_as: None,
};
pub const DEBUG: ModuleSpec = ModuleSpec {
    name: "debug",
    free_form: false,
    summary: "Print a message or the value of a variable.",
    args: &[],
    validated_as: None,
};
pub const FAIL: ModuleSpec = ModuleSpec {
    name: "fail",
    free_form: false,
    summary: "Fail the task with a message.",
    args: &[honoured("msg")],
    validated_as: None,
};
/// `prompt` is read, but nothing here reads an answer to it, so a prompt shown on a terminal is
/// refused when the task runs rather than shown and never waited for.
pub const PAUSE: ModuleSpec = ModuleSpec {
    name: "pause",
    free_form: false,
    summary: "Wait for `seconds` or `minutes`. On a terminal a pause with no duration asks for an answer and keeps it in `user_input`; `prompt` next to a duration is not supported yet. Without a terminal, a pause that asks for input prints a warning and continues at once, as Ansible does.",
    args: &[
        honoured("echo"),
        honoured("minutes"),
        honoured("prompt"),
        honoured("seconds"),
    ],
    validated_as: None,
};
pub const SET_FACT: ModuleSpec = ModuleSpec {
    name: "set_fact",
    free_form: false,
    summary: "Set facts for a host, for the rest of the run.",
    args: &[],
    validated_as: None,
};

pub const INCLUDE_VARS: ModuleSpec = ModuleSpec {
    name: "include_vars",
    free_form: true,
    summary: "Read a file of variables and set them on the host, for the rest of the run.",
    args: &[],
    validated_as: None,
};

pub const VALIDATE_ARGUMENT_SPEC: ModuleSpec = ModuleSpec {
    name: "validate_argument_spec",
    free_form: false,
    summary: "Check a role's arguments against the specification in `meta/argument_specs.yml`.",
    args: &[],
    validated_as: None,
};

/// Modules the controller runs itself and never sends to a host, sorted by name.
pub const LOCAL_MODULES: &[ModuleSpec] = &[
    ASSERT,
    DEBUG,
    FAIL,
    INCLUDE_VARS,
    PAUSE,
    SET_FACT,
    VALIDATE_ARGUMENT_SPEC,
];

/// The three statements that read a file while the play is being compiled instead of naming
/// work for a host, with whether their string form is one raw argument.
///
/// They are written as modules and the loader reads them as modules, but nothing sends them
/// anywhere: the compiler splices what they name into the step list and no step is left behind.
/// `import_role` is the odd one out on the free-form column - measured, `import_playbook: x.yml`
/// takes the file as a raw parameter while `import_role` refuses one and wants `name=`.
pub const IMPORT_MODULES: &[(&str, bool)] = &[
    ("import_playbook", true),
    ("import_role", false),
    ("import_tasks", true),
];

/// Whether the module is one of the three import statements, and whether its string form is raw.
pub fn import_module(module: &str) -> Option<bool> {
    let short = short_name(module);
    IMPORT_MODULES
        .iter()
        .find(|(name, _)| *name == short)
        .map(|(_, free_form)| *free_form)
}

/// The two statements that name work read while the play is **running** rather than while it is
/// being compiled, with whether their string form is one raw argument.
///
/// They are the imports' dynamic twins: what they name is not known until the host that reaches
/// them has rendered its own variables, so the coordinator reads it then and splices the steps in
/// behind the statement. `include_role` refuses a raw parameter and wants `name=`, the way
/// `import_role` does - measured on ansible-core 2.19.12.
///
/// `include_vars` is not here: it is an ordinary controller-side module in [`LOCAL_MODULES`],
/// because what it produces is variables rather than steps.
pub const INCLUDE_MODULES: &[(&str, bool)] = &[("include_role", false), ("include_tasks", true)];

/// Whether the module is one of the two include statements, and whether its string form is raw.
pub fn include_module(module: &str) -> Option<bool> {
    let short = short_name(module);
    INCLUDE_MODULES
        .iter()
        .find(|(name, _)| *name == short)
        .map(|(_, free_form)| *free_form)
}
