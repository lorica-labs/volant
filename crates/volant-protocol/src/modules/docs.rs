// SPDX-License-Identifier: GPL-3.0-or-later

//! The module reference page, generated from the tables.

use std::fmt::Write as _;

use super::{
    ACTION_PLUGINS, ArgStatus, BUILTIN_MODULES, LOCAL_MODULES, ModuleSpec, NATIVE_MODULES, Route,
    builtin_route,
};

/// The Markdown table published in the documentation, generated so it cannot drift.
pub fn documentation_table() -> String {
    let mut out = String::from(
        "---\ntitle: Modules\ndescription: Which modules Volant runs, where each one runs, and which ones are not supported yet.\n---\n\n<!-- Generated from crates/volant-protocol/src/modules.rs by `just docs-modules`. Do not edit by hand. -->\n\nVolant runs a module in one of three places: natively in the agent on the host, on the controller itself, or on the host through the warm Python path. A playbook that names a module this release does not support yet stops when it loads, before the first task, the way Ansible stops on a module it cannot resolve.\n\nA module named in a file that only a dynamic `include_tasks` or `include_role` brings in is checked when a host reaches that statement. The statement then fails for that host and nothing in the file runs. See [Includes and imports](/playbooks/includes/).\n\n## On the agent\n\nThe agent runs these on the host, without Python. The arguments column lists what each module reads.\n\nAnsible declares `chdir`, `creates` and `removes` as paths and expands `~` and `$VAR` in them before using the value, and `command` and `shell` do the same, from the task's `environment` and then the agent's own. The words of a `command` are expanded as Ansible expands them, from the task's `environment` and then the agent's own, so `command: /bin/echo $HOME` prints the home directory unless `expand_argument_vars` is false. A `shell` task has its shell do the expanding, and `expand_argument_vars` is refused there, as it is in Ansible.\n\n| Module | Free-form | Arguments | What it does |\n|---|---|---|---|\n",
    );
    // The internal keys carry the command line itself rather than being written in a playbook,
    // so they are in the registry and out of the page.
    let honoured = |m: &ModuleSpec| {
        m.args
            .iter()
            .filter(|a| a.status == ArgStatus::Honoured && !a.name.starts_with('_'))
            .map(|a| format!("`{}`", a.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    // A refusal tied to one value names that value: the other one is what this release does.
    let refused = |m: &ModuleSpec| {
        m.args
            .iter()
            .filter(|a| !a.name.starts_with('_'))
            .filter_map(|a| match a.status {
                ArgStatus::Refused(Some(v)) => Some(format!("`{}: {v}`", a.name)),
                ArgStatus::Refused(None) => Some(format!("`{}`", a.name)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let rows = |specs: &[ModuleSpec], args: bool, out: &mut String| {
        for m in specs {
            let _ = write!(
                out,
                "| `{}` | {} |",
                m.name,
                if m.free_form { "yes" } else { "no" }
            );
            if args {
                let read = honoured(m);
                let _ = write!(
                    out,
                    " {} |",
                    if read.is_empty() { "-" } else { read.as_str() }
                );
            }
            let _ = writeln!(out, " {} |", m.summary);
        }
    };
    rows(NATIVE_MODULES, true, &mut out);
    let notes: String = NATIVE_MODULES
        .iter()
        .filter_map(|m| {
            let names = refused(m);
            (!names.is_empty()).then(|| format!("- `{}`: {names}\n", m.name))
        })
        .collect();
    if !notes.is_empty() {
        out.push_str(
            "\nThese arguments exist in Ansible and are not supported yet: a playbook that uses them stops before the run reaches a host.\n\n",
        );
        out.push_str(&notes);
    }
    out.push_str(
        "\nOver `ssh`, `raw` runs its command on a terminal, as ansible-core does with `ssh -tt`. The command's stderr is merged into stdout, every line ends in a carriage return and a newline, and stderr holds the line `ssh` prints when the session closes: `Shared connection to <host> closed.`, or `Connection to <host> closed.` when the connection is not shared. Set `ansible_ssh_use_tty` to false to run without a terminal. A local connection has none.\n",
    );
    out.push_str(
        "\n## On the controller\n\nThe controller runs these itself, so they need no connection to the host.\n\n| Module | Free-form | What it does |\n|---|---|---|\n",
    );
    rows(LOCAL_MODULES, false, &mut out);
    out.push_str(
        "
## On the warm Python path

Every other module ansible-core ships is a Python module, and Volant runs it as one. The host needs a Python 3 interpreter, and the controller needs ansible-core to build the payload. [The warm Python path](/internals/python/) explains how it works.

Some of these modules also have a native version in the agent. It answers when the task's arguments stay within what it implements. Otherwise, before it changes anything on the host, it hands the task to the Python module. The table below says which modules have one.

Modules from collections installed on the controller run the same way. The controller's ansible-core resolves each name before the first connection, and Volant never installs a collection. If a play or one of its roles names a module from a collection that is not installed, the run stops before it reaches a host and prints the `ansible-galaxy collection install` command that fixes it. Volant refuses, by name, a module that its collection runs through an action plugin, because it does not run a collection's action plugins. A module that only a dynamic include brings in is refused when a host reaches the include, which then fails for that host. Volant keeps a collection's module under its full name, so that module never stands in for a builtin module or for another collection's module with the same short name. [Modules from collections](/internals/python/#modules-from-collections) has the details.

The exceptions are the modules ansible-core runs through an action plugin that Volant does not have yet. What the playbook asks for lives in the plugin, not in the module, so sending the module alone would do something else and call it a success. Those modules are not supported yet: Volant names them before the run reaches a host. Fact gathering still works: a play's `gather_facts` runs the `setup` module directly.

| Module | How it runs |
|---|---|
",
    );
    for m in BUILTIN_MODULES {
        let how = match builtin_route(m) {
            Some(Route::NativeOrPython) => {
                "natively in the agent when its arguments allow it, as a Python module otherwise"
                    .to_string()
            }
            Some(Route::Python) => "Python module".to_string(),
            _ => continue,
        };
        let _ = writeln!(out, "| `{m}` | {how} |");
    }
    out.push_str(
        "\n## Through an action plugin\n\nThe controller runs these as the reference's action plugins do: it picks or renders what reaches the host, then sends it as ordinary modules over the connection the task already has. `reboot` also waits for the host: it reconnects until the host answers with a new boot id. [Action plugins](/reference/action-plugins/) describes each one.\n\n| Module | What it does |\n|---|---|\n",
    );
    for (m, summary) in ACTION_PLUGINS {
        let _ = writeln!(out, "| `{m}` | {summary} |");
    }
    out
}
