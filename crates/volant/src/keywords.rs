// SPDX-License-Identifier: GPL-3.0-or-later
//! Every play and task keyword ansible-core 2.19 knows, with what this release does about it.
//!
//! The loader accepts everything listed here and refuses anything else, the way the reference
//! does. A keyword is `Runs` here **only when something in this release honours it today**: one
//! marked `Runs` before its executor exists would be accepted by the loader and then ignored. Whoever
//! implements a keyword flips its row in the same change, and the table's own tests then prove
//! the new behaviour in both directions. A keyword this release answers without doing the whole
//! of what the reference does with it is [`Support::Partial`], and carries the sentence the
//! generated page prints about what is left out.
//!
//! The two lists are the reference's own, read out of `Task.fattributes` and
//! `Play.fattributes` on ansible-core 2.19.12, plus the spellings the reference resolves
//! before it builds those dictionaries: `action` and `local_action`, and one `with_*` name per
//! lookup plugin `ansible.builtin` ships. The internal `async_val` and `loop_with` entries are
//! not spellings a playbook can use and are left out.

use std::fmt::Write as _;

use Support::{Partial, Runs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Something in this release honours it: it runs, or it is refused by its own name with
    /// the reference's code when the value asks for what we cannot do.
    Runs,
    /// Accepted and answered, but not to the whole of what the reference does with it. The
    /// generated page says what is missing, because "runs" here would let an operator plan
    /// around a behaviour this release does not have.
    Partial,
}

#[derive(Debug, Clone, Copy)]
pub struct Keyword {
    pub name: &'static str,
    pub support: Support,
    /// What a [`Support::Partial`] row does not do, in the words the generated page prints.
    /// `None` on every other row.
    pub missing: Option<&'static str>,
    /// What the generated page says beside a [`Support::Runs`] row whose reach has a limit
    /// worth knowing. `None` on every other row.
    pub note: Option<&'static str>,
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
        missing: None,
        note: None,
        barrier: false,
    }
}

/// A keyword this release answers without doing the whole of what the reference does with it.
/// `missing` is the sentence the generated page prints next to it.
const fn partial_kw(name: &'static str, missing: &'static str) -> Keyword {
    Keyword {
        name,
        support: Partial,
        missing: Some(missing),
        note: None,
        barrier: false,
    }
}

/// What `async` does not run yet: everything the agent does not start as a module of its own.
const ASYNC_MISSING: &str = "runs on modules the agent runs, native or Python. It is refused \
    before the run on the controller's own modules, on the action plugins this release runs on \
    the controller (`package`, `service` and `dnf` among them, which the reference runs as jobs), \
    on statements, and on `raw` and `async_status`, which the reference refuses too. \
    `async_dir` in `ansible.cfg` is not read; `ansible_async_dir` and `ANSIBLE_ASYNC_DIR` are.";

/// A keyword this release honours, with the one limit the generated page prints next to it.
const fn noted_kw(name: &'static str, note: &'static str) -> Keyword {
    Keyword {
        name,
        support: Runs,
        missing: None,
        note: Some(note),
        barrier: false,
    }
}

/// The limit of the `debugger` rows.
pub const DEBUGGER_NOTE: &str = "`p` and a line that is not a command evaluate a subset of Python: names, literals, indexing, attributes, assignments and the calls the prompt needs. `task_vars` edited at the prompt reach a redo through `u` only";

/// A keyword whose presence on a task makes that task a synchronisation point.
const fn barrier_kw(name: &'static str, support: Support) -> Keyword {
    Keyword {
        name,
        support,
        missing: None,
        note: None,
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
/// `until`, `retries` and `delay` run on a task and appear in no other table: the reference's
/// `Block.fattributes` has none of them, so a block carrying one is refused at load.
pub const TASK_KEYWORDS: &[Keyword] = &[
    kw("action", Runs),
    kw("any_errors_fatal", Runs),
    kw("args", Runs),
    partial_kw("async", ASYNC_MISSING),
    kw("become", Runs),
    kw("become_exe", Runs),
    kw("become_flags", Runs),
    kw("become_method", Runs),
    kw("become_user", Runs),
    kw("changed_when", Runs),
    kw("check_mode", Runs),
    kw("collections", Runs),
    kw("connection", Runs),
    noted_kw("debugger", DEBUGGER_NOTE),
    kw("delay", Runs),
    kw("delegate_facts", Runs),
    kw("delegate_to", Runs),
    kw("diff", Runs),
    kw("environment", Runs),
    kw("failed_when", Runs),
    kw("ignore_errors", Runs),
    kw("ignore_unreachable", Runs),
    kw("local_action", Runs),
    kw("loop", Runs),
    kw("loop_control", Runs),
    kw("module_defaults", Runs),
    kw("name", Runs),
    kw("no_log", Runs),
    kw("notify", Runs),
    kw("poll", Runs),
    kw("port", Runs),
    kw("register", Runs),
    kw("remote_user", Runs),
    kw("retries", Runs),
    barrier_kw("run_once", Runs),
    kw("tags", Runs),
    kw("throttle", Runs),
    kw("timeout", Runs),
    kw("until", Runs),
    kw("vars", Runs),
    kw("when", Runs),
    // One row per lookup plugin `ansible.builtin` ships, because the reference accepts
    // `with_<lookup>` for every one of them and refuses any other `with_*` name outright. A row
    // runs once its lookup exists and its `with_` form has been measured: the loop walks what the
    // lookup returns as a list. The loader and `prepare` read any `with_*` row that runs this way.
    kw("with_config", Runs),
    kw("with_csvfile", Runs),
    kw("with_dict", Runs),
    kw("with_env", Runs),
    kw("with_file", Runs),
    kw("with_fileglob", Runs),
    kw("with_first_found", Runs),
    kw("with_indexed_items", Runs),
    kw("with_ini", Runs),
    kw("with_inventory_hostnames", Runs),
    kw("with_items", Runs),
    kw("with_lines", Runs),
    kw("with_list", Runs),
    kw("with_nested", Runs),
    kw("with_password", Runs),
    kw("with_pipe", Runs),
    kw("with_random_choice", Runs),
    kw("with_sequence", Runs),
    kw("with_subelements", Runs),
    kw("with_template", Runs),
    kw("with_together", Runs),
    kw("with_unvault", Runs),
    kw("with_url", Runs),
    kw("with_varnames", Runs),
    kw("with_vars", Runs),
];

/// Play keywords, alphabetical, as `Play.fattributes` lists them.
///
/// `gather_facts` runs: a play that asks for facts opens with the reference's own `setup`
/// module, built on the controller and run on the host through the payload path, and what it
/// returns lands under `ansible_facts` and flat, as the host's own words. `gather_subset`,
/// `gather_timeout` and `fact_path` are the arguments of that `setup`, checked when the play
/// starts. `vars_prompt` is asked when the play starts too, and `-e` answers it first.
/// `strategy` counts as `Runs`: `linear`, `free`, `host_pinned` and `debug` are what the engine
/// does, and any other strategy is refused by its own name.
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
///
/// `any_errors_fatal`, `ignore_unreachable` and `ignore_errors` are task keywords a play may
/// carry, so the compiler folds them into every task like `no_log`. `max_fail_percentage` is the
/// play's alone: the coordinator ends a batch that lost more of its hosts than the limit allows.
pub const PLAY_KEYWORDS: &[Keyword] = &[
    kw("any_errors_fatal", Runs),
    kw("become", Runs),
    kw("become_exe", Runs),
    kw("become_flags", Runs),
    kw("become_method", Runs),
    kw("become_user", Runs),
    kw("check_mode", Runs),
    kw("collections", Runs),
    kw("connection", Runs),
    noted_kw("debugger", DEBUGGER_NOTE),
    kw("diff", Runs),
    kw("environment", Runs),
    kw("fact_path", Runs),
    kw("force_handlers", Runs),
    kw("gather_facts", Runs),
    kw("gather_subset", Runs),
    kw("gather_timeout", Runs),
    kw("handlers", Runs),
    kw("hosts", Runs),
    kw("ignore_errors", Runs),
    kw("ignore_unreachable", Runs),
    kw("max_fail_percentage", Runs),
    kw("module_defaults", Runs),
    kw("name", Runs),
    kw("no_log", Runs),
    kw("order", Runs),
    kw("port", Runs),
    kw("post_tasks", Runs),
    kw("pre_tasks", Runs),
    kw("remote_user", Runs),
    kw("roles", Runs),
    barrier_kw("run_once", Runs),
    kw("serial", Runs),
    kw("strategy", Runs),
    kw("tags", Runs),
    kw("tasks", Runs),
    kw("throttle", Runs),
    kw("timeout", Runs),
    kw("vars", Runs),
    kw("vars_files", Runs),
    kw("vars_prompt", Runs),
];

/// `loop_control` sub-keys, as `LoopControl.fattributes` lists them on ansible-core 2.19.12.
///
/// A keyword whose value is a mapping needs a table of its own, or the split between loading
/// and refusing reopens one level down: a sub-key read by nobody would be accepted, waved past
/// the pre-flight and dropped, so the run would report success having ignored what the operator
/// wrote. A sub-key this release could not honour is refused where it is read.
pub const LOOP_CONTROL_KEYWORDS: &[Keyword] = &[
    kw("break_when", Runs),
    kw("extended", Runs),
    kw("extended_allitems", Runs),
    kw("index_var", Runs),
    kw("label", Runs),
    kw("loop_var", Runs),
    kw("pause", Runs),
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
    kw("any_errors_fatal", Runs),
    kw("become", Runs),
    kw("become_exe", Runs),
    kw("become_flags", Runs),
    kw("become_method", Runs),
    kw("become_user", Runs),
    kw("block", Runs),
    kw("check_mode", Runs),
    kw("collections", Runs),
    kw("connection", Runs),
    noted_kw("debugger", DEBUGGER_NOTE),
    kw("delegate_facts", Runs),
    kw("delegate_to", Runs),
    kw("diff", Runs),
    kw("environment", Runs),
    kw("ignore_errors", Runs),
    kw("ignore_unreachable", Runs),
    kw("module_defaults", Runs),
    kw("name", Runs),
    kw("no_log", Runs),
    kw("notify", Runs),
    kw("port", Runs),
    kw("remote_user", Runs),
    kw("rescue", Runs),
    barrier_kw("run_once", Runs),
    kw("tags", Runs),
    kw("throttle", Runs),
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

mod page;
pub use page::documentation;

#[cfg(test)]
mod tests;
