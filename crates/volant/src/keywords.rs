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

use std::fmt::Write as _;

use Support::{Preflight, Runs};

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
    /// Whether a task carrying this keyword is a synchronisation point: the hosts of the batch
    /// meet in front of it before any of them runs it.
    ///
    /// Declared here rather than inferred from the task's text, because the two keywords that
    /// need a boundary do not spell one. `run_once` runs on one host and every other host reads
    /// what it produced, so the election has to be unanimous and the readers have to wait; no
    /// substring in the task says so. The textual scan over `hostvars`, `ansible_play_hosts` and
    /// `ansible_play_batch` stays alongside it and catches what a keyword cannot: a task that
    /// reads another host's variables without any keyword at all. A step is a boundary when
    /// either says so.
    pub barrier: bool,
}

const fn kw(name: &'static str, support: Support) -> Keyword {
    Keyword {
        name,
        support,
        barrier: false,
    }
}

/// A keyword whose presence on a task makes that task a synchronisation point.
const fn barrier_kw(name: &'static str, support: Support) -> Keyword {
    Keyword {
        name,
        support,
        barrier: true,
    }
}

/// Whether `name` is a keyword some table declares a synchronisation point. Asked by name so
/// the step loop reads the table rather than a second list that could drift from it.
pub fn is_barrier(name: &str) -> bool {
    [
        TASK_KEYWORDS,
        PLAY_KEYWORDS,
        LOOP_CONTROL_KEYWORDS,
        BLOCK_KEYWORDS,
        HANDLER_KEYWORDS,
    ]
    .iter()
    .flat_map(|table| table.iter())
    .any(|k| k.name == name && k.barrier)
}

/// Task keywords, alphabetical. The module key is whatever is left over once these are taken.
///
/// `gather_facts` has no row here because it belongs to the play; see [`PLAY_KEYWORDS`].
///
/// `check_mode` runs the way `become_method` and `strategy` do, and only that way: `false` is
/// what this release does and is honoured, and `true` is refused by the pre-flight naming the
/// value. It is a `Runs` row because nothing written under it is accepted and then ignored,
/// not because this release has a check mode.
///
/// `until`, `retries` and `delay` run on a task and appear in no other table: the reference's
/// `Block.fattributes` has none of them, so a block carrying one is refused at load.
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
    kw("check_mode", Runs),
    kw("collections", Preflight),
    kw("connection", Preflight),
    kw("debugger", Preflight),
    kw("delay", Runs),
    kw("delegate_facts", Runs),
    kw("delegate_to", Runs),
    kw("diff", Preflight),
    kw("environment", Runs),
    kw("failed_when", Runs),
    kw("ignore_errors", Runs),
    kw("ignore_unreachable", Preflight),
    kw("local_action", Preflight),
    kw("loop", Runs),
    kw("loop_control", Runs),
    kw("module_defaults", Preflight),
    kw("name", Runs),
    kw("no_log", Runs),
    kw("notify", Runs),
    kw("poll", Preflight),
    kw("port", Preflight),
    kw("register", Runs),
    kw("remote_user", Preflight),
    kw("retries", Runs),
    barrier_kw("run_once", Runs),
    kw("tags", Runs),
    kw("throttle", Preflight),
    kw("timeout", Runs),
    kw("until", Runs),
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
/// order the reference runs them in. `handlers` runs with them: the play's list and every role's
/// `handlers/` directory become one list, and a `notify` on a task or a block reaches it.
/// `force_handlers` runs as well, from the play, from `[defaults]` and from `--force-handlers`.
///
/// `tags` runs everywhere it can be written - on a play, on a block, on a role entry and on a
/// task. The compiler folds the outer tags into every task under them and drops the tasks
/// `--tags` and `--skip-tags` leave out, so a tag decides what runs and what the four listing
/// commands show.
///
/// `serial` runs: the play is cut into batches of hosts and the coordinator plays each of them
/// in turn, banner and handlers included, and the run ends where the reference ends it - at the
/// batch whose live hosts all failed.
pub const PLAY_KEYWORDS: &[Keyword] = &[
    kw("any_errors_fatal", Preflight),
    kw("become", Runs),
    kw("become_exe", Preflight),
    kw("become_flags", Preflight),
    kw("become_method", Runs),
    kw("become_user", Runs),
    kw("check_mode", Runs),
    kw("collections", Preflight),
    kw("connection", Preflight),
    kw("debugger", Preflight),
    kw("diff", Preflight),
    kw("environment", Runs),
    kw("fact_path", Preflight),
    kw("force_handlers", Runs),
    kw("gather_facts", Runs),
    kw("gather_subset", Preflight),
    kw("gather_timeout", Preflight),
    kw("handlers", Runs),
    kw("hosts", Runs),
    kw("ignore_errors", Preflight),
    kw("ignore_unreachable", Preflight),
    kw("max_fail_percentage", Preflight),
    kw("module_defaults", Preflight),
    kw("name", Runs),
    kw("no_log", Runs),
    kw("order", Preflight),
    kw("port", Preflight),
    kw("post_tasks", Runs),
    kw("pre_tasks", Runs),
    kw("remote_user", Preflight),
    kw("roles", Runs),
    barrier_kw("run_once", Runs),
    kw("serial", Runs),
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
/// own names.
pub const BLOCK_KEYWORDS: &[Keyword] = &[
    kw("always", Runs),
    kw("any_errors_fatal", Preflight),
    kw("become", Runs),
    kw("become_exe", Preflight),
    kw("become_flags", Preflight),
    kw("become_method", Runs),
    kw("become_user", Runs),
    kw("block", Runs),
    kw("check_mode", Runs),
    kw("collections", Preflight),
    kw("connection", Preflight),
    kw("debugger", Preflight),
    kw("delegate_facts", Runs),
    kw("delegate_to", Runs),
    kw("diff", Preflight),
    kw("environment", Runs),
    kw("ignore_errors", Runs),
    kw("ignore_unreachable", Preflight),
    kw("module_defaults", Preflight),
    kw("name", Runs),
    kw("no_log", Runs),
    kw("notify", Runs),
    kw("port", Preflight),
    kw("remote_user", Preflight),
    kw("rescue", Runs),
    barrier_kw("run_once", Runs),
    kw("tags", Runs),
    kw("throttle", Preflight),
    kw("timeout", Runs),
    kw("vars", Runs),
    kw("when", Runs),
];

/// The three section keywords of a block, in the order [`BLOCK_KEYWORDS`] lists them. A mapping
/// carrying any of them is a block rather than a task.
pub const BLOCK_SECTIONS: &[&str] = &["always", "block", "rescue"];

/// What a handler carries that a task does not.
///
/// `Handler.fattributes` on ansible-core 2.19.12 is `Task.fattributes` plus this one name, so the
/// table holds the difference rather than a second copy of forty rows that would drift from the
/// first. A key written on a handler is looked up here and then in [`TASK_KEYWORDS`].
pub const HANDLER_KEYWORDS: &[Keyword] = &[kw("listen", Runs)];

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

pub fn handler_keyword(name: &str) -> Option<&'static Keyword> {
    HANDLER_KEYWORDS.iter().find(|k| k.name == name)
}

/// What one table says about `name`, as the published page spells it.
fn status(table: &[Keyword], name: &str) -> &'static str {
    match table.iter().find(|k| k.name == name) {
        Some(k) if k.support == Runs => "runs",
        Some(_) => "refused",
        None => "not accepted",
    }
}

/// The Markdown page published in the documentation, generated from the tables above so it
/// cannot say something the loader and the pre-flight do not do.
///
/// The status is per place a keyword can be written rather than per keyword, because the two
/// differ: `ignore_errors` runs on a block and on a task and is refused on a play, and a single
/// column would have to pick one of the two and be wrong about the other.
pub fn documentation() -> String {
    let mut out = String::from(
        "# Keywords\n\nEvery play, block and task keyword ansible-core 2.19 knows, and what this \
         release does with each one.\n\nA playbook is loaded against the whole grammar, so it \
         parses here as it parses there. What this release cannot execute is refused by name \
         before the first connection, rather than accepted and then ignored. A keyword therefore \
         carries a status per place it can be written:\n\n- `runs`: this release honours it, or \
         refuses by name the one value it cannot do.\n- `refused`: it loads, and the run stops \
         before anything connects.\n- `not accepted`: it cannot be written there, and a playbook \
         that writes it there is refused when it is read, as the reference refuses it.\n\nThis \
         page is generated from the tables in the source, so it cannot drift from them. Run `just \
         docs-keywords` after changing a table.\n\n| Keyword | Play | Block | Task |\n|---|---|---|\
         ---|\n",
    );
    let mut names: Vec<&str> = [TASK_KEYWORDS, PLAY_KEYWORDS, BLOCK_KEYWORDS]
        .iter()
        .flat_map(|table| table.iter())
        .map(|k| k.name)
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let _ = writeln!(
            out,
            "| `{name}` | {} | {} | {} |",
            status(PLAY_KEYWORDS, name),
            status(BLOCK_KEYWORDS, name),
            status(TASK_KEYWORDS, name)
        );
    }
    out.push_str(
        "\n## Handlers\n\nA handler takes every task keyword above, and one of its own.\n\n\
         | Keyword | Status |\n|---|---|\n",
    );
    for k in HANDLER_KEYWORDS {
        let _ = writeln!(
            out,
            "| `{}` | {} |",
            k.name,
            status(HANDLER_KEYWORDS, k.name)
        );
    }
    out.push_str(
        "\n## Under `loop_control`\n\nA sub-key ansible-core does not have is refused when the \
         playbook is read.\n\n| Sub-key | Status |\n|---|---|\n",
    );
    for k in LOOP_CONTROL_KEYWORDS {
        let _ = writeln!(
            out,
            "| `{}` | {} |",
            k.name,
            status(LOOP_CONTROL_KEYWORDS, k.name)
        );
    }
    out
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
            HANDLER_KEYWORDS,
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
        let mut expected: Vec<String> = task_fattributes.iter().map(ToString::to_string).collect();
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

        // `Handler.fattributes` read the same way is `Task.fattributes` plus `listen` and
        // nothing else, which is why this table holds the difference alone.
        let ours: Vec<&str> = HANDLER_KEYWORDS.iter().map(|k| k.name).collect();
        assert_eq!(ours, ["listen"]);
        assert!(
            task_keyword("listen").is_none(),
            "a handler keyword is not a task keyword; a name in both would hide one of the two"
        );
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

    /// `run_once` is the only keyword any table declares a barrier.
    ///
    /// A task record carries its keywords as typed fields, not as a list of names to walk, so
    /// `PlayTask::barrier` asks the table for the one name it knows how to read. A second row
    /// marked `barrier` would look declared and be silently ignored - the keyword accepted and
    /// then not honoured, which is the failure family this crate hunts. So the tables hold one.
    ///
    /// What would make this red: `barrier_kw` used for a second keyword. Whoever adds it has to
    /// teach `PlayTask::barrier` to ask for it in the same commit.
    #[test]
    fn run_once_is_the_only_barrier_keyword() {
        let declared: Vec<&str> = [
            TASK_KEYWORDS,
            PLAY_KEYWORDS,
            LOOP_CONTROL_KEYWORDS,
            BLOCK_KEYWORDS,
            HANDLER_KEYWORDS,
        ]
        .iter()
        .flat_map(|table| table.iter())
        .filter(|k| k.barrier)
        .map(|k| k.name)
        .collect();
        assert!(!declared.is_empty());
        assert!(
            declared.iter().all(|name| *name == "run_once"),
            "{declared:?}"
        );
    }

    /// `is_barrier` has exactly one caller, `PlayTask::barrier`, and it only ever asks about the
    /// literal `"run_once"` - a name that is both present in every table and marked `barrier`.
    /// That single call site can never tell a lookup that ignores its argument, or one that
    /// widens the match from "named and marked" to "named or marked", from the real thing: every
    /// input it is ever given makes both the honest answer and the two broken ones agree.
    ///
    /// What would make this red: `is_barrier` returning the same answer for every name, or
    /// treating a name match alone (without the `barrier` flag) as enough.
    #[test]
    fn is_barrier_reads_both_the_name_and_the_flag() {
        assert!(is_barrier("run_once"));
        // `name` is a real keyword in every table and is not a barrier: a lookup that only
        // checks the name would say yes.
        assert!(!is_barrier("name"));
        // Not a keyword at all: a lookup that ignores its argument would say yes.
        assert!(!is_barrier("not_a_real_keyword"));
    }

    /// What would make this red: a keyword added, removed or flipped between `Runs` and
    /// `Preflight` without regenerating the page, which would leave the site telling operators
    /// that something works when the pre-flight refuses it, or the other way round.
    #[test]
    fn the_keyword_page_matches_the_tables() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/src/keywords.md");
        let expected = documentation();
        if std::env::var_os("VOLANT_UPDATE_DOCS").is_some() {
            std::fs::write(&path, &expected).unwrap();
        }
        let actual = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            actual, expected,
            "run `just docs-keywords` to regenerate docs/src/keywords.md"
        );
    }
}
