// SPDX-License-Identifier: GPL-3.0-or-later
//! What a run refuses before it touches a host.
//!
//! The loader accepts the whole ansible-core grammar, so everything it accepted and this
//! release cannot execute has to be caught here, before the first connection and before any
//! banner. A keyword that falls through both gates is silently ignored, and the run then
//! reports success having skipped what the operator asked for - which is the failure the
//! split between loading and refusing exists to prevent.
//!
//! Listing and syntax-checking never call this, so a playbook can be read ahead of the release
//! that runs it.
//!
//! **The pre-flight refuses what the playbook says, not what the tags leave.** `check` reads the
//! play's own lists as written, so a task `--tags` will drop is refused all the same;
//! `check_steps` reads the compilation, where the selection has already removed a role's dropped
//! task, so that one is not. The asymmetry is deliberate and it errs in the safe direction -
//! more refusals, never fewer, and nothing that runs escapes one. Evening it out downward, by
//! dropping the first pass, would let a keyword this release cannot execute reach a run; evening
//! it out upward would mean compiling every play twice. Neither is worth it, and a later change
//! that "fixes" this by weakening the first pass is a regression, not a cleanup.

use anyhow::bail;
use volant_protocol::modules::{import_module, include_module, is_builtin, is_known};

use crate::action_plugins;
use crate::playbook::{Play, PlayTask, Playbook, TaskOrBlock, is_meta};
use crate::stats::Refusal;

mod refusals;
mod resolved;

pub(crate) use refusals::PROMPT_REFUSED;
use refusals::{check_arguments, check_end_role, check_meta, check_pause, refuse_retries};
use resolved::unresolved;
pub(crate) use resolved::{
    check_built, check_collection_actions, check_resolved, collection_modules, refusal_of,
    unresolvable_here,
};

/// The strategies this engine implements: the reference's four. A play asking for another one is
/// refused by its name rather than run under `linear`, which orders tasks across hosts
/// differently.
pub const STRATEGIES: [&str; 4] = ["linear", "free", "host_pinned", "debug"];

/// Exit 4 for the refusals raised here: the reference exits 4 for a playbook it cannot load,
/// and a playbook this release cannot run is the same event from the operator's chair, refused
/// at the same moment. [`Refusal::or`] leaves a member refusal its own code, which is how the
/// escalation refusal below keeps the 2 it measured; anything raised here that has its own
/// measured code must be built with [`Refusal::at`] rather than left to this blanket.
const CODE: i32 = 4;

pub fn check(playbook: &Playbook) -> anyhow::Result<()> {
    for (i, play) in playbook.plays.iter().enumerate() {
        check_play(play).map_err(|e| Refusal::or(CODE, e.context(format!("play {}", i + 1))))?;
    }
    Ok(())
}

fn check_play(play: &Play) -> anyhow::Result<()> {
    if let Some(strategy) = &play.strategy
        && !STRATEGIES.contains(&strategy.as_str())
    {
        bail!("strategy '{strategy}' is not supported yet");
    }
    // A handler is a task the play wrote, so the first pass refuses what it carries too. Only
    // the `notify` names wait for the second pass, because resolving one needs the whole
    // compiled play - a role read from disk contributes handlers this list has never seen.
    for handler in &play.handlers {
        check_task(&handler.task)?;
    }
    check_items(&play.pre_tasks)?;
    check_items(&play.tasks)?;
    check_items(&play.post_tasks)
}

/// The compiled steps of one play, for everything the play's own lists do not hold: a role's
/// tasks, and the tasks of every file an `import_tasks` spliced in.
///
/// Roles turn the pre-flight into two passes rather than one. The first walks what the playbook
/// says, the second what the compilation made of it, and the second is the one that sees a
/// keyword written in a file the playbook only names. A module a role uses and this release
/// cannot run is refused here, before the first connection, exactly as one written in the play
/// is.
///
/// `prompts` says whether a prompting `pause` could block this play: standard input is a
/// terminal and the play matched at least one host. A play that matches none is skipped whole
/// (`skipping: no hosts matched`), so none of its pauses is reached and none is refused.
pub(crate) fn check_steps(
    compiled: &crate::compile::Compiled,
    prompts: bool,
) -> anyhow::Result<()> {
    for step in &compiled.steps {
        // A flush point the compiler put in itself carries no task at all, so there is no module
        // and no `meta` action to judge. No handler step exists yet - the coordinator splices
        // those in as the run reaches each flush - so the handlers themselves are checked below,
        // off `compiled.handlers`. Everything else goes through, `meta` included: a role's
        // `meta: end_play` has to be refused here, since the first pass never saw that file.
        if !matches!(step.kind, crate::compile::StepKind::Flush { .. }) {
            check_task(&step.task).map_err(|e| Refusal::or(CODE, e))?;
        }
        check_end_role(step)?;
        check_pause(compiled, step, prompts).map_err(|e| Refusal::or(CODE, e))?;
    }
    for handler in &compiled.handlers {
        check_task(&handler.task).map_err(|e| Refusal::or(CODE, e))?;
    }
    Ok(())
}

/// What one dynamic `include_tasks` or `include_role` brought in, checked before it is spliced
/// into the running play.
///
/// The two passes above run once, over the compilation, and these two statements read their
/// content while the play runs - so everything they bring in was never there to be checked, and a
/// keyword this release refuses was accepted inside an included file and then ignored. That is the
/// silent skip this module exists to stop, reaching a run by the one door the compilation does not
/// hold.
///
/// It is the same [`check_task`] the two passes use, so a keyword is refused inside an included
/// file by the sentence it is refused by in the play, and a row added to any table closes both
/// doors at once. What differs is what happens next. Exit 4 before the first connection is not
/// available here: hosts are connected and earlier tasks have run. So the refusal fails the
/// statement for the host that asked, rescuable and `ignore_errors`-able, which is what a role
/// nobody can find and a file that is not a list of tasks already do at the same place.
///
/// The whole expansion is refused rather than the one task carrying the keyword, which is the
/// granularity the static path has: a keyword written in a file `import_tasks` names refuses the
/// whole run. Letting the tasks in front of the refused one run first is the half-run the
/// pre-flight exists to prevent, one level down.
///
/// No `notify` is looked up here or in the passes above: a name may be a template, and the
/// reference itself finds a name nothing answers to only when the task changes something. The
/// driver does it then, against the play's handlers. Nor does the pre-flight's `pause` check run
/// here: a `pause` an include brings in is refused by `pause()` when it is reached.
pub(crate) fn check_spliced(expanded: &crate::compile::Compiled) -> anyhow::Result<()> {
    for step in &expanded.steps {
        // As in `check_steps`: a flush point the compiler put in itself carries no task to judge.
        if !matches!(step.kind, crate::compile::StepKind::Flush { .. }) {
            check_task(&step.task)?;
        }
    }
    // An `include_role` hands its role's handlers to the running play, where they wait for a
    // flush. Nothing else looks at them either.
    for handler in &expanded.handlers {
        check_task(&handler.task)?;
    }
    Ok(())
}

/// A task list, blocks and all. Every section of a block is walked, `rescue` included: a
/// keyword this release cannot execute has to be refused wherever it was written, and a
/// playbook whose recovery path carries one would otherwise fail on it only once something had
/// already gone wrong.
fn check_items(items: &[TaskOrBlock]) -> anyhow::Result<()> {
    for item in items {
        match item {
            TaskOrBlock::Task(task) => check_task(task)?,
            TaskOrBlock::Block(block) => {
                check_items(&block.body)?;
                check_items(&block.rescue)?;
                check_items(&block.always)?;
            }
        }
    }
    Ok(())
}

/// One task, checked on its own so the compiler can call it per step once roles put tasks
/// somewhere other than a play's own list.
pub fn check_task(task: &PlayTask) -> anyhow::Result<()> {
    if is_meta(task) {
        return check_meta(task);
    }
    // The three import statements never reach a host: the compiler reads what they name and
    // splices it in, so what has to be checked is the tasks that came out of them, which the
    // second pass over the compiled steps sees. `import_playbook` written as a task is refused
    // there too, with the code the reference measures for it.
    if import_module(&task.module).is_some() {
        return Ok(());
    }
    // The two dynamic statements never reach a host either, and what they name is not known until
    // the host that reaches them has rendered its own variables - so there is nothing here for
    // the second pass to walk. What can be checked before the first connection is their
    // arguments, and the compiler checks those where it turns the statement into a step.
    if include_module(&task.module).is_some() {
        // `until`, `retries` and `delay` never reach a retry on a statement this engine expands:
        // the step loop resolves an include and splices behind it well before `prepare`, which is
        // where the retry plan is built. Accepted here they would be read and dropped, which is
        // the family this pre-flight exists to close.
        //
        // The reference refuses them too. Measured on ansible-core 2.19.12:
        // `'until' is not a valid attribute for a TaskInclude` at exit 4 for `include_tasks`, the
        // same sentence naming `IncludeRole` for `include_role`, because `VALID_INCLUDE_KEYWORDS`
        // holds sixteen names and none of these three. Same refusal, one moment earlier, which is
        // this pre-flight's usual divergence.
        //
        // `include_vars` is not one of these statements, there or here: it is an ordinary
        // controller-side module on both sides, and both honour the three. Measured on the
        // reference and on this engine, the same shape each time - two `FAILED - RETRYING` lines
        // and `attempts: 2`.
        return refuse_retries(task);
    }
    // What the loader kept of a string argument that is not `key=value` fails the task when it
    // runs, as the reference's does (`raw_params_refusal`). A string that is one template is the
    // reference's argument splat, which renders to the module's arguments and which this release
    // does not do: refused here, where a task is checked rather than at load, so a role file for
    // another platform - parsed for the union and never checked - cannot refuse a Linux run.
    if crate::playbook::stray_raw_params(task).is_some_and(crate::playbook::possibly_all_template) {
        bail!(
            "task '{}': module '{}' takes its arguments from one template, which this release does not render into arguments: write them as a mapping",
            task.name,
            task.module
        );
    }
    // A plugin this release runs. `until`, `retries` and `delay` run on it: each attempt replays
    // the plugin's whole sequence of sub-tasks and `until` reads the result it ended with, as a
    // module's retry does - `k3s_server` retries a `remote_src` copy three times.
    if action_plugins::kind(&task.module).is_some() {
        return Ok(());
    }
    if !is_known(&task.module) {
        // A module ansible-core ships, that this release runs neither natively nor on the
        // controller, and that the arm above did not take: it runs through the warm Python path,
        // built on the controller and sent to the agent as a payload. Accepted here, and the run
        // refuses before the first connection if the controller cannot build payloads at all.
        //
        // What is left below is a name no collection has: the operator has a typo to fix, and
        // the reference's own sentence carries it.
        if is_builtin(&task.module) {
            // Asked rather than spelled out a second time. The set that reaches a host is
            // `python::is_python_module` and nothing else, so a name held back there - the fact
            // modules, today - is refused here in the same breath rather than admitted before
            // the first connection and then failing per host for want of a payload.
            if !crate::python::is_python_module(&task.module) {
                bail!(
                    "task '{}': module '{}' is not available in this release",
                    task.name,
                    task.module
                );
            }
            // No argument check here: a Python module validates its own arguments on the host,
            // and `check_arguments` knows only the native modules.
            return Ok(());
        }
        // A collection's module, or a name a `library/` may hold, is set aside rather than
        // judged: only the controller's ansible-core knows which collections are installed and
        // what its module path holds, and it is asked once, after both passes, before the union
        // is built ([`check_resolved`]). A word left in `_raw_params` fails the task when it
        // runs (`raw_params_refusal`), once the name resolved: `shel: echo hi` is a typo before
        // it is a module given raw params.
        if crate::python::is_resolved_name(&task.module) {
            return Ok(());
        }
        bail!(unresolved(&task.name, &task.module));
    }
    check_arguments(task)
}

#[cfg(test)]
mod tests;
