// SPDX-License-Identifier: GPL-3.0-or-later
//! The native modules and what the controller needs to know about them before the agent runs
//! them. Written once here; the agent's implementation table and the documentation are
//! checked against it.

use std::fmt::Write as _;

use serde_json::Value;

/// What one module does with one of its arguments.
///
/// A module being in [`NATIVE_MODULES`] says it runs; it says nothing about the rest of its API,
/// and an argument accepted and then dropped reports success having done something other than
/// what the playbook asked for. The status is per argument so that neither answer is a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgStatus {
    /// Read, and the value decides what runs.
    ///
    /// It says the argument is read, not that it is coerced the way the reference coerces it.
    /// `chdir`, `creates` and `removes` are `type='path'` there, which expands `~` and `$VAR`
    /// before the value is used, and this release takes the value as it is written.
    Honoured,
    /// The reference accepts the name on this module and acts on it nowhere, which is what this
    /// release does with it too - `executable` on `command`, where no shell runs for it to
    /// select. Nothing to promise and nothing to refuse, so it is out of the generated page.
    Inert,
    /// The reference has it and this release does not do what it asks for. Named by the
    /// pre-flight, before the first connection, rather than accepted and forgotten.
    ///
    /// The value carried is the one that is refused, the way `check_mode` is refused for `true`
    /// and runs for `false`: the other value asks for what this release already does, and
    /// refusing that would stop a playbook the two engines agreed on. `None` refuses the name
    /// whatever the value says.
    Refused(Option<bool>),
}

/// A value read the way the reference reads an argument declared `type='bool'`.
///
/// Measured on ansible-core 2.19.12, whose own refusal lists the spellings it takes: `0, 1, 'n',
/// 'on', 'true', 'f', 'false', 'y', 'yes', 'no', '0', '1', 't', 'off'`, and a string outside that
/// set fails the task there. It normalises with `.lower().strip()`, so the value is case folded
/// **and** trimmed before it is matched: a trailing newline off a `lookup('file')`, or the space a
/// template leaves behind, reads the same there as the bare word. `Value::as_bool` sees none of
/// them, so an argument written `"false"` - which a whole-expression template produces on its own
/// - used to read back as the default and do the opposite of what it says.
///
/// This is not the set the YAML scalar resolver uses, and the two must not be merged: that one
/// has no `y`, `n`, `t` or `f`, and matches three fixed capitalisations where this one folds
/// case. The `bool` filter has a third set of its own. Three readers, three measurements, one
/// place each.
pub fn arg_bool(value: &Value) -> Option<bool> {
    let text = match value {
        Value::Bool(b) => return Some(*b),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_ascii_lowercase(),
        _ => return None,
    };
    match text.as_str() {
        "y" | "yes" | "on" | "1" | "1.0" | "true" | "t" => Some(true),
        "n" | "no" | "off" | "0" | "0.0" | "false" | "f" => Some(false),
        _ => None,
    }
}

/// One argument of one module, and what this release does with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleArg {
    pub name: &'static str,
    pub status: ArgStatus,
}

/// One native module: its Ansible name and how the controller parses its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleSpec {
    /// Short name, without the `ansible.builtin.` prefix.
    pub name: &'static str,
    /// The task's string form is a command line kept whole as `_raw_params`,
    /// instead of `key=value` pairs.
    pub free_form: bool,
    /// One line for the generated documentation.
    pub summary: &'static str,
    /// Every argument the reference accepts for this module, sorted, with what this release does
    /// with each. Empty for a module whose arguments nothing here checks yet.
    pub args: &'static [ModuleArg],
    /// The name the reference validates this module's arguments under, when it validates them at
    /// all, and so the name its refusal carries.
    ///
    /// Measured on ansible-core 2.19.12: `shell` is executed by the `command` module, so an
    /// argument neither module has is refused as `ansible.legacy.command` under both names and
    /// against one list. `raw` never reaches a module at all - its action plugin hands the line
    /// to the shell - so it takes any argument without looking at it, and a run that refused one
    /// would refuse a playbook the reference runs.
    pub validated_as: Option<&'static str>,
}

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
        // Measured: it decides whether the arguments handed to the program have their shell
        // variables expanded first - `/bin/echo $HOME` prints the home directory by default and the
        // five characters `$HOME` when it is off. This release expands nothing, which is what `false`
        // asks for, so on `command` only `true` is refused: refusing `false` would stop a playbook
        // that ran the same under both engines. The default is the value that diverges, and it is
        // the one nobody writes; `docs/src/modules.md` says so, because no refusal can.
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
const COMMAND_ARGS: [ModuleArg; 12] =
    command_args(ArgStatus::Inert, ArgStatus::Refused(Some(true)));
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

/// Every module the agent implements natively, sorted by name.
pub const NATIVE_MODULES: &[ModuleSpec] = &[COMMAND, RAW, SHELL];

pub const DEBUG: ModuleSpec = ModuleSpec {
    name: "debug",
    free_form: false,
    summary: "Print a message or the value of a variable.",
    args: &[],
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
pub const LOCAL_MODULES: &[ModuleSpec] = &[DEBUG, INCLUDE_VARS, SET_FACT, VALIDATE_ARGUMENT_SPEC];

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

/// `ansible.builtin.` and `ansible.legacy.` name the same modules as the bare name does.
/// Other collections are returned whole: `community.general.command` is not our `command`.
pub fn short_name(module: &str) -> &str {
    module
        .strip_prefix("ansible.builtin.")
        .or_else(|| module.strip_prefix("ansible.legacy."))
        .unwrap_or(module)
}

pub fn native(module: &str) -> Option<&'static ModuleSpec> {
    let short = short_name(module);
    NATIVE_MODULES.iter().find(|m| m.name == short)
}

pub fn local(module: &str) -> Option<&'static ModuleSpec> {
    let short = short_name(module);
    LOCAL_MODULES.iter().find(|m| m.name == short)
}

/// Every module `ansible.builtin` ships in ansible-core 2.19.12, as `ansible-doc -l -t module
/// ansible.builtin` lists them, sorted and short-named. A playbook naming one of these and
/// none of `NATIVE_MODULES` or `LOCAL_MODULES` is a playbook this release cannot run yet,
/// which the operator needs to hear differently from a name that resolves to no module at all:
/// the first waits on us, the second is a typo.
pub const BUILTIN_MODULES: &[&str] = &[
    "add_host",
    "apt",
    "apt_key",
    "apt_repository",
    "assemble",
    "assert",
    "async_status",
    "blockinfile",
    "command",
    "copy",
    "cron",
    "deb822_repository",
    "debconf",
    "debug",
    "dnf",
    "dnf5",
    "dpkg_selections",
    "expect",
    "fail",
    "fetch",
    "file",
    "find",
    "gather_facts",
    "get_url",
    "getent",
    "git",
    "group",
    "group_by",
    "hostname",
    "import_playbook",
    "import_role",
    "import_tasks",
    "include_role",
    "include_tasks",
    "include_vars",
    "iptables",
    "known_hosts",
    "lineinfile",
    "meta",
    "mount_facts",
    "package",
    "package_facts",
    "pause",
    "ping",
    "pip",
    "raw",
    "reboot",
    "replace",
    "rpm_key",
    "script",
    "service",
    "service_facts",
    "set_fact",
    "set_stats",
    "setup",
    "shell",
    "slurp",
    "stat",
    "subversion",
    "systemd",
    "systemd_service",
    "sysvinit",
    "tempfile",
    "template",
    "unarchive",
    "uri",
    "user",
    "validate_argument_spec",
    "wait_for",
    "wait_for_connection",
    "yum_repository",
];

/// Whether `module` names a builtin, bare or under the two prefixes that mean the same thing.
/// Another collection's module is never one, however familiar the short name looks: if
/// `community.general.command` were taken for a builtin, a playbook naming it would be told to
/// wait for a release that will never contain it.
pub fn is_builtin(module: &str) -> bool {
    let ours = !module.contains('.')
        || module.starts_with("ansible.builtin.")
        || module.starts_with("ansible.legacy.");
    ours && BUILTIN_MODULES.contains(&short_name(module))
}

/// Whether this engine can run the module at all, natively on the agent or on the controller.
/// The two tables are all there is, so a playbook naming anything else can be refused while it
/// is being loaded, which is where the reference refuses a module it cannot resolve.
pub fn is_known(module: &str) -> bool {
    native(module).is_some() || local(module).is_some()
}

/// The Markdown table published in the documentation, generated so it cannot drift.
pub fn documentation_table() -> String {
    let mut out = String::from(
        "# Modules\n\nThese are the modules Volant runs. A playbook naming any other module is refused when it is loaded, before the first task, the way Ansible refuses a module it cannot resolve. A file a dynamic `include_tasks` or `include_role` names is read when a host reaches the statement, so a module named there is refused at that moment instead: the statement fails for the host that asked, and nothing in the file runs. What `import_tasks` and `import_role` name is compiled with the play and checked with it. Everything else waits on the warm Python path.\n\n## On the agent\n\nThe agent runs these on the host, without Python. The arguments column lists what each module reads, and each is read as the playbook writes it. Ansible declares `chdir`, `creates` and `removes` as paths, which expands `~` and `$VAR` in them before the value is used; this release does not, so `creates: ~/.provisioned` looks for a directory named `~`. No other argument is expanded either. Ansible runs `command: /bin/echo $HOME` with the variable already replaced, and here the program is handed the five characters `$HOME`. A `shell` task prints the same thing under both engines, because there the shell does the expanding rather than the module. The list under the table names the arguments Ansible has and this release refuses.\n\n| Module | Free-form arguments | Arguments | What it does |\n|---|---|---|---|\n",
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
        out.push_str("\nVolant refuses these before the run reaches a host:\n\n");
        out.push_str(&notes);
    }
    out.push_str(
        "\n## On the controller\n\nThe controller runs these itself, so they need no connection to the host.\n\n| Module | Free-form arguments | What it does |\n|---|---|---|\n",
    );
    rows(LOCAL_MODULES, false, &mut out);
    // A rule rather than a list. The names this section covers are every builtin minus the two
    // tables above minus the ones an action plugin backs, and that last list lives in the
    // controller crate: this crate is the one both the controller and the agent depend on, so it
    // cannot read it. Naming seventy modules here and being wrong about twenty of them would be
    // worse than saying what decides.
    out.push_str(
        "\n## On the warm Python path\n\nEverything else ansible-core ships is a Python module, and Volant runs it as one. The modules a run needs travel to the host together, once, in a single archive named by its own content. A Python server the agent keeps warm runs each of them in a fork of itself. The agent keeps the archive, so a host that already has it is sent nothing, and the interpreter comes from the list the agent reported when it started.\n\nWhich modules those are follows a rule rather than a list: every builtin in neither table above, except the ones the reference runs through an action plugin. Volant refuses those by name before the run reaches a host, because what the playbook asks for lives in the plugin and not in the module. `package` picks the host's package manager, and `template` renders the file on the controller before the task is sent. Sending the module on its own would run something else and call it a success.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn builtin_prefixes_are_stripped_and_collections_kept() {
        assert_eq!(short_name("ansible.builtin.command"), "command");
        assert_eq!(short_name("ansible.legacy.shell"), "shell");
        assert_eq!(short_name("command"), "command");
        assert_eq!(short_name("community.general.ufw"), "community.general.ufw");
    }

    #[test]
    fn native_lookup_uses_the_short_name() {
        assert_eq!(native("ansible.builtin.raw").map(|m| m.name), Some("raw"));
        assert!(native("file").is_none(), "file is not native yet");
        assert!(
            native("community.general.command").is_none(),
            "another collection's command is not ours"
        );
    }

    #[test]
    fn names_are_unique_and_sorted() {
        for table in [NATIVE_MODULES, LOCAL_MODULES] {
            let names: Vec<&str> = table.iter().map(|m| m.name).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            assert_eq!(names, sorted);
        }
        let mut all: Vec<&str> = NATIVE_MODULES
            .iter()
            .chain(LOCAL_MODULES)
            .map(|m| m.name)
            .collect();
        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), total, "a module name is in both tables");
    }

    #[test]
    fn only_the_two_tables_are_known() {
        assert!(is_known("ansible.builtin.shell"));
        assert!(is_known("set_fact"));
        assert!(is_known("ansible.legacy.debug"));
        assert!(!is_known("nosuchmodule"));
        assert!(!is_known("file"), "not implemented yet, so not known");
        assert!(!is_known("community.general.debug"));
    }

    /// The three dynamic statements, each in the table that answers for it.
    ///
    /// `include_vars` is a controller-side module - it produces variables, and a result line with
    /// them - while `include_tasks` and `include_role` produce steps and never run as modules at
    /// all. Told apart here because the pre-flight reads exactly this, and what it says next
    /// depends on which of them it is looking at: a name an action plugin backs is refused by
    /// that name first of all, a builtin in neither table is answered with the sentence about
    /// this release, and a name no collection has gets the reference's own words about a
    /// misspelling. All three statements were refused outright before the coordinator could
    /// splice, and none of them reaches any of those sentences now.
    ///
    /// What would make this red: `include_vars` left out of the controller-side table, which
    /// refuses a playbook this release now runs; or either statement added to it, which would
    /// send a step naming work to `run_local` and fail it as "not a controller-side module".
    #[test]
    fn the_three_dynamic_statements_are_each_in_one_table() {
        assert!(is_known("include_vars"));
        assert!(is_known("ansible.builtin.include_vars"));
        assert!(include_module("include_vars").is_none());
        for statement in ["include_tasks", "include_role"] {
            assert!(include_module(statement).is_some(), "{statement}");
            assert!(!is_known(statement), "{statement}");
        }
        assert_eq!(include_module("include_tasks"), Some(true));
        assert_eq!(include_module("ansible.builtin.include_role"), Some(false));
    }

    /// The three states a module name can be in have to stay three. Collapsing them - which an
    /// `&&`/`||` precedence slip does in one character - would tell an operator with a typo to
    /// wait for a release, or an operator waiting for `lineinfile` that they misspelled it.
    ///
    /// What would make this red: `is_builtin` answering true for a name no collection has, or
    /// false for one `ansible.builtin` ships.
    #[test]
    fn a_builtin_is_told_apart_from_a_name_that_resolves_to_nothing() {
        for yes in [
            "lineinfile",
            "ansible.builtin.lineinfile",
            "ansible.legacy.file",
            "command",
        ] {
            assert!(is_builtin(yes), "{yes}");
        }
        for no in [
            "nosuchmodule",
            "ansible.builtin.nosuchmodule",
            "community.general.ufw",
            "community.general.command",
        ] {
            assert!(!is_builtin(no), "{no}");
        }
        assert!(
            !is_known("lineinfile") && is_builtin("lineinfile"),
            "a builtin we have not written is neither runnable nor a typo"
        );
    }

    /// What would make this red: a native or controller-side module added under a name
    /// `ansible.builtin` does not have, which would make it unreachable for anyone writing the
    /// reference's spelling.
    #[test]
    fn the_builtin_table_is_sorted_and_covers_both_registries() {
        let mut sorted = BUILTIN_MODULES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(BUILTIN_MODULES, sorted.as_slice());
        for m in NATIVE_MODULES.iter().chain(LOCAL_MODULES) {
            assert!(
                BUILTIN_MODULES.contains(&m.name),
                "{} is not a name ansible.builtin has",
                m.name
            );
        }
        for table in [IMPORT_MODULES, INCLUDE_MODULES] {
            let names: Vec<&str> = table.iter().map(|(n, _)| *n).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(names, sorted);
            for name in names {
                assert!(BUILTIN_MODULES.contains(&name), "{name}");
                assert!(
                    !is_known(name),
                    "{name} names work rather than being it, so nothing runs it as a module"
                );
            }
        }
    }

    /// Every argument list is sorted, free of duplicates, and total: a module the reference
    /// validates carries every name the reference's own refusal lists, because that refusal is
    /// built from this list and a name missing here would be quoted as unsupported while the
    /// reference supports it.
    ///
    /// What would make this red: a name appended rather than inserted in order, which puts the
    /// reference's sentence out of order too; a name listed twice, which prints it twice; or a
    /// module gaining a `validated_as` with nothing to validate against, which would refuse every
    /// argument a playbook writes.
    #[test]
    fn every_argument_list_is_sorted_and_covers_what_the_reference_validates() {
        for m in NATIVE_MODULES.iter().chain(LOCAL_MODULES) {
            let names: Vec<&str> = m.args.iter().map(|a| a.name).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(names, sorted, "{}", m.name);
            assert!(
                m.validated_as.is_none() || !names.is_empty(),
                "{} validates against an empty list",
                m.name
            );
        }
        // Measured on ansible-core 2.19.12, through `args:` on both modules: one module name,
        // one list of names, and one refusal. The ad-hoc path validates nothing, because
        // `free_form` swallows the whole line into `_raw_params`.
        let names = |m: &ModuleSpec| m.args.iter().map(|a| a.name).collect::<Vec<_>>();
        assert_eq!(
            names(&COMMAND),
            names(&SHELL),
            "the reference shares the module"
        );
        assert_eq!(SHELL.validated_as, Some("ansible.legacy.command"));
        assert_eq!(
            RAW.validated_as, None,
            "raw's action plugin never validates an argument"
        );
    }

    /// `executable` names the shell a command line is handed to, so it means something only where
    /// a shell runs. The reference accepts it on `command` and starts no shell there, and so does
    /// this release - which is parity, not a gap, and so belongs in neither column of the page.
    ///
    /// What would make this red: `command` claiming to read it, which the generated page then
    /// tells an operator, who writes it and gets no shell, no refusal and no hint why.
    #[test]
    fn executable_is_read_only_where_a_shell_runs() {
        let status = |m: &ModuleSpec| {
            m.args
                .iter()
                .find(|a| a.name == "executable")
                .map(|a| a.status)
        };
        assert_eq!(status(&COMMAND), Some(ArgStatus::Inert));
        assert_eq!(status(&SHELL), Some(ArgStatus::Honoured));
        assert_eq!(status(&RAW), Some(ArgStatus::Honoured));
        let page = documentation_table();
        let row = |name: &str| {
            page.lines()
                .find(|l| l.starts_with(&format!("| `{name}` |")))
                .unwrap_or_default()
                .to_string()
        };
        assert!(!row("command").contains("executable"), "{}", row("command"));
        assert!(row("shell").contains("`executable`"), "{}", row("shell"));
    }

    /// `expand_argument_vars` is a `command` argument, and the two modules answer for it
    /// differently because the reference does.
    ///
    /// Measured on ansible-core 2.19.12: on `command` the default expands and `false` does not,
    /// so `true` is the only value that asks for something this release cannot do. On `shell`
    /// the reference has no such argument at all and fails the task with `Unsupported parameters
    /// for (shell) module: expand_argument_vars` whatever the value says, while a `shell` line
    /// with the argument left out expands its variables under both engines, the shell doing it
    /// rather than the module.
    ///
    /// What would make this red: `shell` carrying `command`'s value-tied refusal, which lets
    /// `expand_argument_vars: false` through on a task the reference refuses; or `command`
    /// refusing the name, which stops a playbook that ran the same under both engines.
    #[test]
    fn expand_argument_vars_is_refused_by_the_value_on_one_module_and_by_name_on_the_other() {
        let status = |m: &ModuleSpec| {
            m.args
                .iter()
                .find(|a| a.name == "expand_argument_vars")
                .map(|a| a.status)
        };
        assert_eq!(status(&COMMAND), Some(ArgStatus::Refused(Some(true))));
        assert_eq!(status(&SHELL), Some(ArgStatus::Refused(None)));
        assert_eq!(status(&RAW), None);
        let page = documentation_table();
        assert!(
            page.contains("- `command`: `expand_argument_vars: true`\n"),
            "{page}"
        );
        assert!(
            page.contains("- `shell`: `expand_argument_vars`\n"),
            "{page}"
        );
    }

    /// The divergence no refusal can name is the default, so the page has to name it instead.
    /// This is the argument slice of the same class of gap the refusals close: an argument read
    /// otherwise than the reference reads it, with the playbook saying nothing about it.
    ///
    /// What would make this red: the sentence dropped from the generated page, which leaves an
    /// operator writing `creates: ~/.provisioned` or `command: mkdir -p $HOME/releases` with
    /// nothing published to read it against.
    #[test]
    fn the_page_says_arguments_are_read_as_written() {
        let page = documentation_table();
        for phrase in [
            "read as the playbook writes it",
            "expands `~` and `$VAR`",
            "`command: /bin/echo $HOME`",
        ] {
            assert!(page.contains(phrase), "{phrase} is missing from:\n{page}");
        }
    }

    /// The spellings the reference takes for an argument declared `type='bool'`, read from its
    /// own refusal of a value outside the set and measured task by task: `"false"`, `"no"`, `0`
    /// and `"Off"` all turn the argument off there.
    ///
    /// The reference normalises with `.lower().strip()`, so surrounding whitespace is part of no
    /// spelling: `"false "` is `false` there, and a value coming out of a template or a
    /// `lookup('file')` carries that whitespace more often than a hand-written one does.
    ///
    /// What would make this red: reading the value with `Value::as_bool`, which answers `None`
    /// for every spelling but a YAML boolean and leaves the caller on its default - so a task
    /// asking for the opposite of the default gets the default and reports success; or matching
    /// the string untrimmed, which does the same to every value with a newline on the end.
    #[test]
    fn a_boolean_argument_reads_every_spelling_the_reference_takes() {
        for yes in [
            json!(true),
            json!("true"),
            json!("True"),
            json!("YES"),
            json!("t"),
            json!("on"),
            json!(1),
            json!(1.0),
            json!(" true"),
        ] {
            assert_eq!(arg_bool(&yes), Some(true), "{yes}");
        }
        for no in [
            json!(false),
            json!("false"),
            json!("no"),
            json!("Off"),
            json!("f"),
            json!(0),
            json!("0"),
            json!("false "),
            json!("no\n"),
        ] {
            assert_eq!(arg_bool(&no), Some(false), "{no}");
        }
        for neither in [
            json!("maybe"),
            json!(2),
            json!(null),
            json!([true]),
            json!(""),
        ] {
            assert_eq!(arg_bool(&neither), None, "{neither}");
        }
    }

    #[test]
    fn the_documentation_table_matches_the_registry() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/src/modules.md");
        let expected = documentation_table();
        if std::env::var_os("VOLANT_UPDATE_DOCS").is_some() {
            std::fs::write(&path, &expected).unwrap();
        }
        let actual = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            actual, expected,
            "run `just docs-modules` to regenerate docs/src/modules.md"
        );
    }
}
