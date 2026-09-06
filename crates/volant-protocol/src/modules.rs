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

/// The Markdown table published in the documentation, generated so it cannot drift.
pub fn documentation_table() -> String {
    let mut out = String::from(
        "# Native modules\n\nModules the agent runs without Python. Every other module runs through the warm Python path once it exists.\n\n| Module | Free-form arguments | What it does |\n|---|---|---|\n",
    );
    for m in NATIVE_MODULES {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            m.name,
            if m.free_form { "yes" } else { "no" },
            m.summary
        ));
    }
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
        let names: Vec<&str> = NATIVE_MODULES.iter().map(|m| m.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted);
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
