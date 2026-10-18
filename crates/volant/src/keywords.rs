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
    /// Loaded and kept, so a playbook parses as it does in the reference, then refused by the
    /// pre-flight before the first connection rather than ignored during the run.
    Preflight,
}

#[derive(Debug, Clone, Copy)]
pub struct Keyword {
    pub name: &'static str,
    pub support: Support,
    /// What a [`Support::Partial`] row does not do, in the words the generated page prints.
    /// `None` on every other row.
    pub missing: Option<&'static str>,
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
        barrier: false,
    }
}

/// What `async` does not run yet: everything the agent does not start as a module of its own.
const ASYNC_MISSING: &str = "runs on modules the agent runs, native or Python. It is refused \
    before the run on the controller's own modules, on the action plugins this release runs on \
    the controller (`package`, `service` and `dnf` among them, which the reference runs as jobs), \
    on statements, and on `raw` and `async_status`, which the reference refuses too. \
    `async_dir` in `ansible.cfg` is not read; `ansible_async_dir` and `ANSIBLE_ASYNC_DIR` are.";

/// One sentence for the `debugger` rows. The reference's prompt also evaluates Python.
const DEBUGGER_MISSING: &str = "the prompt takes `c` (continue) and `q` (quit) only; `p`, `r`, `u` and the rest are not supported yet, and a standard input that is not a terminal is the end of input";

/// A keyword whose presence on a task makes that task a synchronisation point.
const fn barrier_kw(name: &'static str, support: Support) -> Keyword {
    Keyword {
        name,
        support,
        missing: None,
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
    partial_kw("debugger", DEBUGGER_MISSING),
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
    partial_kw("debugger", DEBUGGER_MISSING),
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
/// wrote. A sub-key this release could not honour would be parked here as `Preflight`.
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
    partial_kw("debugger", DEBUGGER_MISSING),
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

/// What one table says about `name`, as the published page spells it, with the class the site
/// draws it by.
fn status(table: &[Keyword], name: &str) -> &'static str {
    match table.iter().find(|k| k.name == name) {
        Some(k) if k.support == Runs => r#"<span class="kw kw-runs">runs</span>"#,
        Some(k) if k.support == Partial => r#"<span class="kw kw-partial">partial</span>"#,
        Some(_) => r#"<span class="kw kw-notyet">not yet</span>"#,
        None => r#"<span class="kw kw-no">not accepted</span>"#,
    }
}

/// The page published in the documentation, generated from the tables above so it cannot say
/// something the loader and the pre-flight do not do. The legend and the main grid are HTML, so the
/// site can draw each status as a shape and filter the grid; everything reads without scripts.
///
/// The status is per place a keyword can be written rather than per keyword, because the two
/// differ: `ignore_errors` runs on a block and on a task and is not supported on a play, and a
/// single column would have to pick one of the two and be wrong about the other.
pub fn documentation() -> String {
    let mut out = String::from(
        "---\ntitle: Keywords\ndescription: Every play, block and task keyword ansible-core 2.19 \
         knows, and what Volant does with each one.\n---\n\n<!-- Generated from \
         crates/volant/src/keywords.rs by `just docs-keywords`. Do not edit by hand. -->\n\n\
         Volant loads a playbook against the whole ansible-core 2.19 grammar, so it parses here \
         as it parses under Ansible. What this release does not support yet is named before the \
         run starts, never accepted and then ignored. Each keyword has a status for each place it \
         can be written.\n\n\
         <ul class=\"kw-legend not-content\">\n\
         <li><span class=\"kw kw-runs\">runs</span><span>Volant honors it, or names the one value \
         it does not support yet.</span></li>\n\
         <li><span class=\"kw kw-partial\">partial</span><span>Accepted and honored, but not \
         everything Ansible does with it. The gap is listed below the grid.</span></li>\n\
         <li><span class=\"kw kw-notyet\">not yet</span><span>Not supported yet. The playbook \
         loads, and the run stops before anything connects. In a file a dynamic include names, \
         the statement fails when a host reaches it instead.</span></li>\n\
         <li><span class=\"kw kw-no\">not accepted</span><span>Cannot be written there. Ansible \
         rejects it too, when it reads the playbook.</span></li>\n\
         </ul>\n\n\
         A keyword inside a file that only a dynamic `include_tasks` or `include_role` names is \
         checked when a host reaches that statement: if it is not supported yet, the statement \
         fails for that host, a `rescue` around it can catch the failure, and nothing in the file \
         runs. See [Includes and imports](/playbooks/includes/).\n\n\
         This page is generated from the tables the loader and the pre-flight read, so it always \
         describes the release it ships with.\n\n\
         ## All keywords\n\n\
         <div class=\"kw-grid-wrap not-content\">\n<table class=\"kw-grid\">\n<thead><tr>\
         <th>Keyword</th><th>Play</th><th>Block</th><th>Task</th></tr></thead>\n<tbody>\n",
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
            "<tr><td><code>{name}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            status(PLAY_KEYWORDS, name),
            status(BLOCK_KEYWORDS, name),
            status(TASK_KEYWORDS, name)
        );
    }
    out.push_str("</tbody>\n</table>\n</div>\n");
    let mut partial: Vec<(&str, &str)> = [
        TASK_KEYWORDS,
        PLAY_KEYWORDS,
        LOOP_CONTROL_KEYWORDS,
        BLOCK_KEYWORDS,
        HANDLER_KEYWORDS,
    ]
    .iter()
    .flat_map(|table| table.iter())
    .filter_map(|k| k.missing.map(|m| (k.name, m)))
    .collect();
    partial.sort_unstable();
    partial.dedup();
    out.push_str(
        "\n## What partial leaves out\n\nEach keyword below behaves the same way in every place \
         the grid marks it `partial`.\n\n| Keyword | What is missing |\n|---|---|\n",
    );
    for (name, missing) in partial {
        let _ = writeln!(out, "| `{name}` | {missing} |");
    }

    out.push_str(
        "\n## Handlers\n\nA handler accepts every task keyword above, plus one of its own.\n\n\
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
        "\n## Under loop_control\n\nA sub-key ansible-core does not know is rejected when the \
         playbook loads.\n\n| Sub-key | Status |\n|---|---|\n",
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
mod tests;
