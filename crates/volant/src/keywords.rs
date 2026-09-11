// SPDX-License-Identifier: GPL-3.0-or-later
//! Every play and task keyword ansible-core 2.19 knows, with what this release does about it.
//!
//! The loader accepts everything listed here and refuses anything else, the way the reference
//! does; the pre-flight then refuses, before the first connection, everything marked
//! [`Support::Preflight`]. Splitting the one old rule ("a keyword we do not implement is
//! refused by name") in two only stays safe while both halves are true, so a keyword is
//! `Runs` here **only when something in this release honours it today**. A keyword marked
//! `Runs` before its executor exists would be accepted by the loader, waved through by the
//! pre-flight and then ignored, which is the failure this split exists to prevent. Whoever
//! implements a keyword flips its row in the same change, and the table's own tests then prove
//! the new behaviour in both directions.
//!
//! The two lists are the reference's own, read out of `Task.fattributes` and
//! `Play.fattributes` on ansible-core 2.19.12, plus the spellings the reference resolves
//! before it builds those dictionaries: `action` and `local_action`, and one `with_*` name per
//! lookup plugin `ansible.builtin` ships. The internal `async_val` and `loop_with` entries are
//! not spellings a playbook can use and are left out.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Something in this release honours it: it runs, or it is refused by its own name with
    /// the reference's code when the value asks for what we cannot do.
    Runs,
    /// Loaded and kept, so a playbook parses as it does in the reference, then refused by the
    /// pre-flight before the first connection rather than ignored during the run.
    Preflight,
}

#[derive(Debug, Clone, Copy)]
pub struct Keyword {
    pub name: &'static str,
    pub support: Support,
}

const fn kw(name: &'static str, support: Support) -> Keyword {
    Keyword { name, support }
}

use Support::{Preflight, Runs};

/// Task keywords, alphabetical. The module key is whatever is left over once these are taken.
///
/// `gather_facts` has no row here because it belongs to the play; see [`PLAY_KEYWORDS`].
pub const TASK_KEYWORDS: &[Keyword] = &[
    kw("action", Preflight),
    kw("any_errors_fatal", Preflight),
    kw("args", Runs),
    kw("async", Preflight),
    kw("become", Runs),
    kw("become_exe", Preflight),
    kw("become_flags", Preflight),
    kw("become_method", Runs),
    kw("become_user", Runs),
    kw("changed_when", Runs),
    kw("check_mode", Preflight),
    kw("collections", Preflight),
    kw("connection", Preflight),
    kw("debugger", Preflight),
    kw("delay", Preflight),
    kw("delegate_facts", Preflight),
    kw("delegate_to", Preflight),
    kw("diff", Preflight),
    kw("environment", Preflight),
    kw("failed_when", Runs),
    kw("ignore_errors", Runs),
    kw("ignore_unreachable", Preflight),
    kw("local_action", Preflight),
    kw("loop", Runs),
    kw("loop_control", Runs),
    kw("module_defaults", Preflight),
    kw("name", Runs),
    kw("no_log", Preflight),
    kw("notify", Preflight),
    kw("poll", Preflight),
    kw("port", Preflight),
    kw("register", Runs),
    kw("remote_user", Preflight),
    kw("retries", Preflight),
    kw("run_once", Preflight),
    kw("tags", Runs),
    kw("throttle", Preflight),
    kw("timeout", Runs),
    kw("until", Preflight),
    kw("vars", Runs),
    kw("when", Runs),
    // One row per lookup plugin `ansible.builtin` ships, because the reference accepts
    // `with_<lookup>` for every one of them and refuses any other `with_*` name outright. They
    // exist there, so they are refused here by the pre-flight rather than mistaken for a
    // module name; only `with_items` has a loop to run.
    kw("with_config", Preflight),
    kw("with_csvfile", Preflight),
    kw("with_dict", Preflight),
    kw("with_env", Preflight),
    kw("with_file", Preflight),
    kw("with_fileglob", Preflight),
    kw("with_first_found", Preflight),
    kw("with_indexed_items", Preflight),
    kw("with_ini", Preflight),
    kw("with_inventory_hostnames", Preflight),
    kw("with_items", Runs),
    kw("with_lines", Preflight),
    kw("with_list", Preflight),
    kw("with_nested", Preflight),
    kw("with_password", Preflight),
    kw("with_pipe", Preflight),
    kw("with_random_choice", Preflight),
    kw("with_sequence", Preflight),
    kw("with_subelements", Preflight),
    kw("with_template", Preflight),
    kw("with_together", Preflight),
    kw("with_unvault", Preflight),
    kw("with_url", Preflight),
    kw("with_varnames", Preflight),
    kw("with_vars", Preflight),
];

/// Play keywords, alphabetical, as `Play.fattributes` lists them.
///
/// `gather_facts` counts as `Runs` because this release answers it rather than ignoring it: it
/// warns that facts are not gathered, which is a line the operator reads before the first task,
/// not a silent skip. `strategy` counts as `Runs` for the same reason: `linear` is what the
/// engine does, and any other strategy is refused by its own name.
///
/// `roles`, `pre_tasks` and `post_tasks` run: the compiler reads each role from disk and splices
/// its tasks, its dependencies and its argument-spec check into the step list, in the section
/// order the reference runs them in. `handlers` stays parked - a role's handlers are read by
/// nothing yet, and `notify` is parked too, so no handler can be reached from anywhere.
///
/// `tags` runs everywhere it can be written - on a play, on a block, on a role entry and on a
/// task. The compiler folds the outer tags into every task under them and drops the tasks
/// `--tags` and `--skip-tags` leave out, so a tag decides what runs and what the four listing
/// commands show.
pub const PLAY_KEYWORDS: &[Keyword] = &[
    kw("any_errors_fatal", Preflight),
    kw("become", Runs),
    kw("become_exe", Preflight),
    kw("become_flags", Preflight),
    kw("become_method", Runs),
    kw("become_user", Runs),
    kw("check_mode", Preflight),
    kw("collections", Preflight),
    kw("connection", Preflight),
    kw("debugger", Preflight),
    kw("diff", Preflight),
    kw("environment", Preflight),
    kw("fact_path", Preflight),
    kw("force_handlers", Preflight),
    kw("gather_facts", Runs),
    kw("gather_subset", Preflight),
    kw("gather_timeout", Preflight),
    kw("handlers", Preflight),
    kw("hosts", Runs),
    kw("ignore_errors", Preflight),
    kw("ignore_unreachable", Preflight),
    kw("max_fail_percentage", Preflight),
    kw("module_defaults", Preflight),
    kw("name", Runs),
    kw("no_log", Preflight),
    kw("order", Preflight),
    kw("port", Preflight),
    kw("post_tasks", Runs),
    kw("pre_tasks", Runs),
    kw("remote_user", Preflight),
    kw("roles", Runs),
    kw("run_once", Preflight),
    kw("serial", Preflight),
    kw("strategy", Runs),
    kw("tags", Runs),
    kw("tasks", Runs),
    kw("throttle", Preflight),
    kw("timeout", Preflight),
    kw("vars", Runs),
    kw("vars_files", Runs),
    kw("vars_prompt", Preflight),
];

/// `loop_control` sub-keys, as `LoopControl.fattributes` lists them on ansible-core 2.19.12.
///
/// A keyword whose value is a mapping needs a table of its own, or the split between loading
/// and refusing reopens one level down: `loop_control` was read for `loop_var` and `label`
/// alone, and `index_var`, `pause`, `break_when`, `extended` and `extended_allitems` were
/// accepted, waved past the pre-flight and dropped, so the run reported success having ignored
/// what the operator wrote. They are parked here instead, and the pre-flight refuses them by
/// their own names.
pub const LOOP_CONTROL_KEYWORDS: &[Keyword] = &[
    kw("break_when", Preflight),
    kw("extended", Preflight),
    kw("extended_allitems", Preflight),
    kw("index_var", Preflight),
    kw("label", Runs),
    kw("loop_var", Runs),
    kw("pause", Preflight),
];

/// Block keywords, alphabetical, as `Block.fattributes` lists them on ansible-core 2.19.12.
///
/// Measured rather than derived: the list is **not** the inherited subset of the task
/// keywords. It carries `delegate_to`, `delegate_facts`, `notify` and `collections`, and it has
/// no `args`, `register`, `loop`, `loop_control`, `until`, `retries`, `delay`, `changed_when`,
/// `failed_when`, `async` or `poll` - which is why `loop` on a block is refused by the
/// reference and by this loader.
///
/// `name` runs because this release honours it the way the reference does: measured, a block's
/// name is shown nowhere, neither as a banner nor in `--list-tasks`, and its tasks keep their
/// own names. `rescue` is parked because nothing enters a rescue section yet: the compiler
/// lays the section out and the driver deliberately steps over it, so a playbook that expects
/// a failure to be recovered is refused rather than run without the recovery it wrote.
pub const BLOCK_KEYWORDS: &[Keyword] = &[
    kw("always", Runs),
    kw("any_errors_fatal", Preflight),
    kw("become", Runs),
    kw("become_exe", Preflight),
    kw("become_flags", Preflight),
    kw("become_method", Runs),
    kw("become_user", Runs),
    kw("block", Runs),
    kw("check_mode", Preflight),
    kw("collections", Preflight),
    kw("connection", Preflight),
    kw("debugger", Preflight),
    kw("delegate_facts", Preflight),
    kw("delegate_to", Preflight),
    kw("diff", Preflight),
    kw("environment", Preflight),
    kw("ignore_errors", Runs),
    kw("ignore_unreachable", Preflight),
    kw("module_defaults", Preflight),
    kw("name", Runs),
    kw("no_log", Preflight),
    kw("notify", Preflight),
    kw("port", Preflight),
    kw("remote_user", Preflight),
    kw("rescue", Preflight),
    kw("run_once", Preflight),
    kw("tags", Runs),
    kw("throttle", Preflight),
    kw("timeout", Runs),
    kw("vars", Runs),
    kw("when", Runs),
];

/// The three section keywords of a block, in the order [`BLOCK_KEYWORDS`] lists them. A mapping
/// carrying any of them is a block rather than a task.
pub const BLOCK_SECTIONS: &[&str] = &["always", "block", "rescue"];

pub fn task_keyword(name: &str) -> Option<&'static Keyword> {
    TASK_KEYWORDS.iter().find(|k| k.name == name)
}

pub fn play_keyword(name: &str) -> Option<&'static Keyword> {
    PLAY_KEYWORDS.iter().find(|k| k.name == name)
}

pub fn loop_control_keyword(name: &str) -> Option<&'static Keyword> {
    LOOP_CONTROL_KEYWORDS.iter().find(|k| k.name == name)
}

pub fn block_keyword(name: &str) -> Option<&'static Keyword> {
    BLOCK_KEYWORDS.iter().find(|k| k.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sorted and free of duplicates, so a missing keyword is easy to spot and the lookups
    /// above cannot be shadowed by a second row. Sorting is by bytes, which is what
    /// `sort_unstable` on `&str` does and what the tables are written in.
    #[test]
    fn the_tables_are_sorted_and_free_of_duplicates() {
        for table in [
            TASK_KEYWORDS,
            PLAY_KEYWORDS,
            LOOP_CONTROL_KEYWORDS,
            BLOCK_KEYWORDS,
        ] {
            let names: Vec<&str> = table.iter().map(|k| k.name).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(names, sorted);
        }
        let mut sections = BLOCK_SECTIONS.to_vec();
        sections.sort_unstable();
        sections.dedup();
        assert_eq!(BLOCK_SECTIONS, sections.as_slice());
    }

    /// `Task.fattributes` and `Play.fattributes` as ansible-core 2.19.12 reports them on the
    /// development machine, minus the two internal spellings a playbook cannot write
    /// (`async_val`, which is what `async` is stored as, and `loop_with`, which is what a
    /// `with_*` key becomes), plus the keys the reference resolves before it builds those
    /// dictionaries: `action`, `local_action` and one name per `ansible.builtin` lookup
    /// plugin.
    ///
    /// What would make this red: a keyword added to a table that the reference does not have,
    /// or one of the reference's dropped. Both are ways of diverging from the grammar the
    /// loader is supposed to accept exactly.
    #[test]
    fn the_tables_are_the_reference_s_own_attribute_lists() {
        let task_fattributes = [
            "action",
            "any_errors_fatal",
            "args",
            "async",
            "become",
            "become_exe",
            "become_flags",
            "become_method",
            "become_user",
            "changed_when",
            "check_mode",
            "collections",
            "connection",
            "debugger",
            "delay",
            "delegate_facts",
            "delegate_to",
            "diff",
            "environment",
            "failed_when",
            "ignore_errors",
            "ignore_unreachable",
            "loop",
            "loop_control",
            "module_defaults",
            "name",
            "no_log",
            "notify",
            "poll",
            "port",
            "register",
            "remote_user",
            "retries",
            "run_once",
            "tags",
            "throttle",
            "timeout",
            "until",
            "vars",
            "when",
        ];
        let lookups = [
            "config",
            "csvfile",
            "dict",
            "env",
            "file",
            "fileglob",
            "first_found",
            "indexed_items",
            "ini",
            "inventory_hostnames",
            "items",
            "lines",
            "list",
            "nested",
            "password",
            "pipe",
            "random_choice",
            "sequence",
            "subelements",
            "template",
            "together",
            "unvault",
            "url",
            "varnames",
            "vars",
        ];
        let mut expected: Vec<String> = task_fattributes.iter().map(|s| s.to_string()).collect();
        expected.push("local_action".to_string());
        expected.extend(lookups.iter().map(|l| format!("with_{l}")));
        expected.sort_unstable();
        let ours: Vec<String> = TASK_KEYWORDS.iter().map(|k| k.name.to_string()).collect();
        assert_eq!(ours, expected);

        let play_fattributes = [
            "any_errors_fatal",
            "become",
            "become_exe",
            "become_flags",
            "become_method",
            "become_user",
            "check_mode",
            "collections",
            "connection",
            "debugger",
            "diff",
            "environment",
            "fact_path",
            "force_handlers",
            "gather_facts",
            "gather_subset",
            "gather_timeout",
            "handlers",
            "hosts",
            "ignore_errors",
            "ignore_unreachable",
            "max_fail_percentage",
            "module_defaults",
            "name",
            "no_log",
            "order",
            "port",
            "post_tasks",
            "pre_tasks",
            "remote_user",
            "roles",
            "run_once",
            "serial",
            "strategy",
            "tags",
            "tasks",
            "throttle",
            "timeout",
            "vars",
            "vars_files",
            "vars_prompt",
        ];
        let ours: Vec<&str> = PLAY_KEYWORDS.iter().map(|k| k.name).collect();
        assert_eq!(ours, play_fattributes);

        // `LoopControl.fattributes`, read the same way. The reference refuses any other
        // sub-key outright, so this list is the whole grammar under `loop_control`.
        let loop_control_fattributes = [
            "break_when",
            "extended",
            "extended_allitems",
            "index_var",
            "label",
            "loop_var",
            "pause",
        ];
        let ours: Vec<&str> = LOOP_CONTROL_KEYWORDS.iter().map(|k| k.name).collect();
        assert_eq!(ours, loop_control_fattributes);

        // `Block.fattributes`, read the same way on the same release: 31 names, `name` among
        // them. Written out rather than derived from the task list, because it is not that
        // list minus the loop keywords: it also has `delegate_to`, `delegate_facts`, `notify`
        // and `collections`, and the derivation that guessed otherwise would have refused
        // `name:` on a block, which the reference runs.
        let block_fattributes = [
            "always",
            "any_errors_fatal",
            "become",
            "become_exe",
            "become_flags",
            "become_method",
            "become_user",
            "block",
            "check_mode",
            "collections",
            "connection",
            "debugger",
            "delegate_facts",
            "delegate_to",
            "diff",
            "environment",
            "ignore_errors",
            "ignore_unreachable",
            "module_defaults",
            "name",
            "no_log",
            "notify",
            "port",
            "remote_user",
            "rescue",
            "run_once",
            "tags",
            "throttle",
            "timeout",
            "vars",
            "when",
        ];
        let ours: Vec<&str> = BLOCK_KEYWORDS.iter().map(|k| k.name).collect();
        assert_eq!(ours, block_fattributes);
    }

    /// A block section is never also a task keyword: the loader tells a block from a task by
    /// looking for one of these keys, so a name in both would make one of the two unreachable.
    /// Every section is a block keyword, which is what makes the block table total.
    #[test]
    fn the_block_sections_are_block_keywords_and_not_task_keywords() {
        for section in BLOCK_SECTIONS {
            assert!(task_keyword(section).is_none(), "{section}");
            assert!(block_keyword(section).is_some(), "{section}");
        }
    }
}
