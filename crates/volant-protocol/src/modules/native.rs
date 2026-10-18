// SPDX-License-Identifier: GPL-3.0-or-later

//! The modules the agent runs without Python, and the Python modules it may run natively.

use super::{ArgStatus, ModuleArg, ModuleSpec, honoured};

/// The parameters the reference's `command` module accepts, sorted, as its own refusal lists
/// them - which is not what `ansible-doc` reports. Measured on ansible-core 2.19.12: the refusal
/// names `_raw_params`, `_uses_shell` and `executable`, which the documentation leaves out, and
/// leaves out `free_form`, which the documentation has. The refusal is the list that decides, so
/// it is the list written here.
///
/// The two internal keys are in it for that reason and not as an exemption: the controller writes
/// `_raw_params` for every free-form task and `_uses_shell` marks the shell semantics, and the
/// reference accepts both by name. `cmd` is the ordinary spelling of the same command line.
///
/// `shell` shares these names because the reference shares the module. Two statuses differ, so
/// the names are written once and the differences are arguments: `executable` is the shell
/// `shell` hands its line to, and on `command` the reference starts no shell for it to select;
/// `expand_argument_vars` is a `command` argument the reference does not give `shell` at all.
const fn command_args(executable: ArgStatus, expand_argument_vars: ArgStatus) -> [ModuleArg; 12] {
    [
        ModuleArg {
            name: "_raw_params",
            status: ArgStatus::Honoured,
        },
        ModuleArg {
            name: "_uses_shell",
            status: ArgStatus::Honoured,
        },
        ModuleArg {
            name: "argv",
            status: ArgStatus::Honoured,
        },
        ModuleArg {
            name: "chdir",
            status: ArgStatus::Honoured,
        },
        ModuleArg {
            name: "cmd",
            status: ArgStatus::Honoured,
        },
        ModuleArg {
            name: "creates",
            status: ArgStatus::Honoured,
        },
        ModuleArg {
            name: "executable",
            status: executable,
        },
        // Measured: on `command` it decides whether the words handed to the program have their
        // `$VAR`, `${VAR}` and `~` expanded first, on by default; `shell` is not given it.
        ModuleArg {
            name: "expand_argument_vars",
            status: expand_argument_vars,
        },
        ModuleArg {
            name: "removes",
            status: ArgStatus::Honoured,
        },
        ModuleArg {
            name: "stdin",
            status: ArgStatus::Honoured,
        },
        ModuleArg {
            name: "stdin_add_newline",
            status: ArgStatus::Honoured,
        },
        ModuleArg {
            name: "strip_empty_ends",
            status: ArgStatus::Honoured,
        },
    ]
}

/// The list `command` answers with: `executable` is accepted and does nothing, there as here.
const COMMAND_ARGS: [ModuleArg; 12] = command_args(ArgStatus::Inert, ArgStatus::Honoured);
/// The same names, with the argument `shell` reads and `command` does not, and the argument the
/// reference gives `command` and refuses on `shell`.
///
/// Measured on ansible-core 2.19.12: `shell: /bin/echo $HOME` with `expand_argument_vars` set to
/// either value answers `Unsupported parameters for (shell) module: expand_argument_vars` and
/// fails the task, while the same line without the argument prints the home directory, because
/// the shell expands it rather than the module. So a `shell` task diverges here for no value of
/// this argument, and writing it at all is a playbook the reference refuses: the name is refused
/// whatever it says, which is the reference's own answer, given before the first connection.
///
/// One spelling goes the other way. `expand_argument_vars: "{{ omit }}"` on `shell` takes the
/// key out of the arguments there and runs the task, while a refusal tied to no value cannot
/// read a template and refuses it here. It is the only value of the only argument where the two
/// disagree in that direction, against every literal value where refusing is what the reference
/// does, and a refusal before the first connection says exactly what it refused.
const SHELL_ARGS: [ModuleArg; 12] = command_args(ArgStatus::Honoured, ArgStatus::Refused(None));

pub const COMMAND: ModuleSpec = ModuleSpec {
    name: "command",
    free_form: true,
    summary: "Run a program directly, without a shell.",
    args: &COMMAND_ARGS,
    validated_as: Some("ansible.legacy.command"),
};
pub const RAW: ModuleSpec = ModuleSpec {
    name: "raw",
    free_form: true,
    summary: "Run a command line through the remote shell, with no module machinery around it.",
    args: &[ModuleArg {
        name: "executable",
        status: ArgStatus::Honoured,
    }],
    validated_as: None,
};
pub const SHELL: ModuleSpec = ModuleSpec {
    name: "shell",
    free_form: true,
    summary: "Run a command line through a shell, `sh` unless `executable` names another.",
    args: &SHELL_ARGS,
    validated_as: Some("ansible.legacy.command"),
};

/// `async_status`, answered in the agent from the job's status file. The reference runs it as an
/// action plugin that adds `_async_dir` and wraps the module's answer; the controller adds the
/// directory here too, and the agent's module gives the wrapped answer itself.
pub const ASYNC_STATUS: ModuleSpec = ModuleSpec {
    name: "async_status",
    free_form: false,
    summary: "Report on a job started with `async`, or remove its status file.",
    args: &[honoured("_async_dir"), honoured("jid"), honoured("mode")],
    validated_as: None,
};

/// Every module the agent implements natively, sorted by name.
pub const NATIVE_MODULES: &[ModuleSpec] = &[ASYNC_STATUS, COMMAND, RAW, SHELL];

/// The arguments ansible-core's `_update_module_args` adds for a task in check mode and for one
/// that shows diffs. The controller sends them, set to `true`, only when they are; the agent
/// takes them out before a module of its own reads the arguments and hands them to a Python one.
pub const CHECK_MODE_ARG: &str = "_ansible_check_mode";
pub const DIFF_ARG: &str = "_ansible_diff";

/// The Python modules the agent may run natively instead, by the reference's name. The agent
/// decides per task: a native answers only inside the subset of arguments it implements and
/// hands anything else back to the Python payload before it touches the host. Which of these an
/// agent actually runs is what it reports in `Ready.natives`.
pub const NATIVE_CANDIDATES: &[&str] = &[
    "setup",
    "stat",
    "file",
    "copy",
    "lineinfile",
    "systemd",
    "systemd_service",
    "apt",
    "user",
    "group",
    "package_facts",
    "service_facts",
];

/// The candidates this release's agent runs natively, aliases included: what the modules page
/// says of them. The agent checks its own table against this list, so the page cannot promise a
/// native the agent does not have, nor miss one it has.
pub const NATIVE_ENABLED: &[&str] = &[
    "setup",
    "stat",
    "file",
    "copy",
    "systemd",
    "systemd_service",
    "apt",
    "user",
    "group",
    "package_facts",
    "service_facts",
    "lineinfile",
];
