// SPDX-License-Identifier: GPL-3.0-or-later
//! The native modules and what the controller needs to know about them before the agent runs
//! them. Written once here; the agent's implementation table and the documentation are
//! checked against it.

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
}

pub const COMMAND: ModuleSpec = ModuleSpec {
    name: "command",
    free_form: true,
    summary: "Run a program directly, without a shell.",
};
pub const RAW: ModuleSpec = ModuleSpec {
    name: "raw",
    free_form: true,
    summary: "Run a command line through the remote shell, with no module machinery around it.",
};
pub const SHELL: ModuleSpec = ModuleSpec {
    name: "shell",
    free_form: true,
    summary: "Run a command line through `sh -c`.",
};

/// Every module the agent implements natively, sorted by name.
pub const NATIVE_MODULES: &[ModuleSpec] = &[COMMAND, RAW, SHELL];

pub const DEBUG: ModuleSpec = ModuleSpec {
    name: "debug",
    free_form: false,
    summary: "Print a message or the value of a variable.",
};
pub const SET_FACT: ModuleSpec = ModuleSpec {
    name: "set_fact",
    free_form: false,
    summary: "Set facts for a host, for the rest of the run.",
};

pub const VALIDATE_ARGUMENT_SPEC: ModuleSpec = ModuleSpec {
    name: "validate_argument_spec",
    free_form: false,
    summary: "Check a role's arguments against the specification in `meta/argument_specs.yml`.",
};

/// Modules the controller runs itself and never sends to a host, sorted by name.
pub const LOCAL_MODULES: &[ModuleSpec] = &[DEBUG, SET_FACT, VALIDATE_ARGUMENT_SPEC];

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
        "# Modules\n\nThese are the modules Volant runs. A playbook naming any other module is refused when it is loaded, before the first task, the way Ansible refuses a module it cannot resolve. Everything else waits on the warm Python path.\n\n## On the agent\n\nThe agent runs these on the host, without Python.\n\n| Module | Free-form arguments | What it does |\n|---|---|---|\n",
    );
    let rows = |specs: &[ModuleSpec], out: &mut String| {
        for m in specs {
            out.push_str(&format!(
                "| `{}` | {} | {} |\n",
                m.name,
                if m.free_form { "yes" } else { "no" },
                m.summary
            ));
        }
    };
    rows(NATIVE_MODULES, &mut out);
    out.push_str(
        "\n## On the controller\n\nThe controller runs these itself, so they need no connection to the host.\n\n| Module | Free-form arguments | What it does |\n|---|---|---|\n",
    );
    rows(LOCAL_MODULES, &mut out);
    out
}

#[cfg(test)]
mod tests {
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
        let imports: Vec<&str> = IMPORT_MODULES.iter().map(|(n, _)| *n).collect();
        let mut sorted = imports.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(imports, sorted);
        for name in imports {
            assert!(BUILTIN_MODULES.contains(&name), "{name}");
            assert!(
                !is_known(name),
                "{name} is spliced by the compiler and never run"
            );
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
