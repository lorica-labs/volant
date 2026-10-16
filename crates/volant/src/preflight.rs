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
use volant_protocol::modules::{
    ArgStatus, import_module, include_module, is_builtin, is_known, short_name,
};

use crate::action_plugins;
use crate::compile::{META_ACTIONS, meta_action};
use crate::playbook::{Play, PlayTask, Playbook, TaskOrBlock, is_meta};
use crate::stats::Refusal;

/// The strategy this engine implements. A play asking for another one is refused by its name
/// rather than run under this one: `free` and `host_pinned` order tasks across hosts
/// differently, and a play written for one of them is not the same play under `linear`.
pub const STRATEGY: &str = "linear";

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
    if let Some(kw) = play.unsupported.first() {
        bail!("play keyword '{kw}' is not supported yet");
    }
    // The play's own `check_mode` reaches every task through the merge, so a play carrying
    // `true` is refused here as well as on each task: a play with no tasks at all would
    // otherwise be accepted by a run that cannot do what it asked for.
    if play.check_mode == Some(true) {
        bail!("play keyword 'check_mode' is not supported yet with 'true'");
    }
    if let Some(strategy) = &play.strategy
        && strategy != STRATEGY
    {
        bail!("strategy '{strategy}' is not supported yet");
    }
    for entry in &play.roles {
        if let Some(kw) = entry.keywords.unsupported.first() {
            bail!("role '{}': keyword '{kw}' is not supported yet", entry.name);
        }
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
                if let Some(kw) = block.keywords.unsupported.first() {
                    bail!(
                        "block '{}': keyword '{kw}' is not supported yet",
                        block.keywords.name
                    );
                }
                // The block's own `check_mode`, before its tasks inherit it: a block with an
                // empty body carries no task for the merged check to reach.
                if block.keywords.check_mode == Some(true) {
                    bail!(
                        "block '{}': keyword 'check_mode' is not supported yet with 'true'",
                        block.keywords.name
                    );
                }
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
    if let Some(kw) = task.unsupported.first() {
        bail!("task '{}': keyword '{kw}' is not supported yet", task.name);
    }
    // `check_mode: false` is what this release does, so it is honoured by doing nothing;
    // `check_mode: true` asks for a mode that reports what a task would have changed without
    // changing it, and running the task for real instead is the worst answer available here.
    // Refused by naming the value, the way an unsupported `become_method` or `strategy` is.
    if task.check_mode == Some(true) {
        bail!(
            "task '{}': keyword 'check_mode' is not supported yet with 'true'",
            task.name
        );
    }
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
    // Before the two sentences below, because a module the reference runs through an action
    // plugin is not a module waiting on this release to write it: the behaviour a playbook asks
    // for lives in the plugin, on the controller, and shipping the module alone would run
    // something else. `template` renders before the task is sent, `copy` reads its source on the
    // controller, and neither is what the payload holds.
    if action_plugins::is_action_backed(&task.module) {
        bail!(
            "task '{}': module '{}' needs an action plugin, which this release does not run yet",
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

/// The reference's sentence for a name nothing answers to.
fn unresolved(task: &str, module: &str) -> String {
    format!("task '{task}': {}", unresolved_tail(module))
}

fn unresolved_tail(module: &str) -> String {
    format!(
        "couldn't resolve module/action '{module}'. This often indicates a misspelling, missing collection, or incorrect module path."
    )
}

/// Every `(task, module)` of a compiled play whose module only the controller's ansible-core can
/// answer, set aside by [`check_task`] for it to resolve.
pub(crate) fn collection_modules(compiled: &crate::compile::Compiled) -> Vec<(String, String)> {
    set_aside(compiled)
        .map(|task| (task.name.clone(), task.module.clone()))
        .collect()
}

fn set_aside(compiled: &crate::compile::Compiled) -> impl Iterator<Item = &PlayTask> {
    compiled
        .steps
        .iter()
        .filter(|step| !matches!(step.kind, crate::compile::StepKind::Flush { .. }))
        .map(|step| &step.task)
        .chain(compiled.handlers.iter().map(|handler| &handler.task))
        .filter(|task| crate::python::is_resolved_name(&task.module))
}

/// What the controller's ansible-core made of a collection's module a task names, refused
/// before the first connection unless it is a module the union can hold.
///
/// Volant never installs a collection, and never runs a collection's action plugin: the plugin
/// is controller code this release does not execute, and sending its module alone would run
/// something the playbook did not ask for. A missing collection is named, with the command that
/// installs it; a module missing from a collection that is installed is the reference's typo
/// sentence alone.
pub(crate) fn check_resolved(
    task: &str,
    module: &str,
    answer: &crate::python::Resolved,
) -> anyhow::Result<()> {
    match refusal_of(module, answer) {
        None => Ok(()),
        Some(why) => Err(Refusal::at(CODE, format!("task '{task}': {why}"))),
    }
}

/// Why a collection's module the controller resolved cannot run, as the refusal says it after
/// the task's name, or `None` for a module the union can hold. Kept in [`crate::python::Union`]
/// for the names a play does not name itself, so an include reaching one says the same.
pub(crate) fn refusal_of(module: &str, answer: &crate::python::Resolved) -> Option<String> {
    use crate::python::Resolved;
    match answer {
        Resolved::Module { .. } => None,
        Resolved::ActionPlugin { fqcn } => Some(format!(
            "module '{fqcn}' needs an action plugin from its collection, which this release does not run"
        )),
        Resolved::Unusable { reason } => Some(format!("module '{module}' cannot run: {reason}")),
        Resolved::Missing { collection: None } => Some(unresolved_tail(module)),
        Resolved::Missing {
            collection: Some(collection),
        } => Some(format!(
            "{} The collection '{collection}' is not installed on the controller: install it with ansible-galaxy collection install {collection}",
            unresolved_tail(module)
        )),
    }
}

/// A name outside `ansible.builtin` a task gives, on a controller where no ansible-core could be
/// asked what it is: the reference's sentence, then why nothing could look.
pub(crate) fn unresolvable_here(task: &str, module: &str, why: &anyhow::Error) -> anyhow::Error {
    Refusal::at(
        CODE,
        format!(
            "{} Only the controller's ansible-core can resolve a name outside ansible.builtin, and {why:#}",
            unresolved(task, module)
        ),
    )
}

/// A module set aside for the controller, refused unless the union holds it. Run on the plays
/// before the first connection, and on what an include brings in. A word the loader kept in
/// `_raw_params` fails the task when it runs (`raw_params_refusal`).
///
/// The union was built before the first connection, from what the plays and their role files
/// name, and a set-aside module is in it only if the controller's ansible-core resolved it to a
/// module it could build. One that is not was missing, is served by an action plugin, or sits
/// in a file nothing read before the run; the include fails for the host that reached it, before
/// any of the file's tasks run, like any other refusal of an included file.
pub(crate) fn check_built(
    expanded: &crate::compile::Compiled,
    union: Option<&crate::python::Union>,
) -> anyhow::Result<()> {
    for task in set_aside(expanded) {
        let (name, module) = (&task.name, &task.module);
        if !union.is_some_and(|u| u.modules.contains_key(crate::python::payload_key(module))) {
            // What the controller made of a name it set aside, as the pre-flight says it for one
            // a play names.
            if let Some(why) = union.and_then(|u| u.refused.get(module)) {
                bail!("task '{name}': {why}");
            }
            bail!(
                "task '{name}': module '{module}' was not resolved to a module before the run, so no payload holds it: its collection is not installed on the controller, no module path holds it, it runs through an action plugin, or it is named only in a file nothing read before the first connection"
            );
        }
    }
    Ok(())
}

/// Why a `pause` that would wait for an answer is refused on a terminal.
pub(crate) const PROMPT_REFUSED: &str = "pause cannot prompt on a terminal because Volant does not read the answer and would carry on without waiting for one. Give the pause seconds or minutes and no prompt, or redirect standard input.";

/// A `pause` that would wait for an answer, refused before the first connection when standard
/// input is a terminal and the step runs whatever happens. The shape is written in the playbook
/// (a `prompt`, or neither `seconds` nor `minutes`), and the terminal is this process's; refused
/// at the task instead, it would stop the run after earlier tasks changed the hosts.
///
/// Only a step that is sure to run is refused here. A `when` on it or on a block around it
/// (folded into the task's own list), a `loop`, or a place inside a `rescue`, can leave the pause
/// unreached; a `never` tag leaves it out of the compiled steps unless `--tags` asks for it, and the reference runs such a playbook to its end; `pause()` refuses the
/// one that is reached, as it does for a `pause` a dynamic include names. Handlers are not
/// steps, so they are left to it too.
fn check_pause(
    compiled: &crate::compile::Compiled,
    step: &crate::compile::Step,
    interactive: bool,
) -> anyhow::Result<()> {
    let task = &step.task;
    if !interactive || short_name(&task.module) != "pause" || !runs_whatever_happens(compiled, step)
    {
        return Ok(());
    }
    let has = |key: &str| task.args.contains_key(key);
    if has("prompt") || !(has("seconds") || has("minutes")) {
        bail!("task '{}': {PROMPT_REFUSED}", task.name);
    }
    Ok(())
}

/// Whether nothing written on a step or around it can leave it out: no `when` (a block's is
/// folded into its tasks), no `loop`, and no `rescue` anywhere up its chain of blocks. A `never` tag needs no
/// test of its own: the compiler drops such a task unless `--tags` selects it, and a selected one
/// runs.
fn runs_whatever_happens(compiled: &crate::compile::Compiled, step: &crate::compile::Step) -> bool {
    // A loop may be over nothing, and then the task never runs.
    if !step.task.when.is_empty() || step.task.loop_items.is_some() {
        return false;
    }
    let (mut section, mut block) = (step.section, step.block);
    loop {
        if section == crate::compile::Section::Rescue {
            return false;
        }
        let Some(index) = block else {
            return true;
        };
        section = compiled.blocks[index].section;
        block = compiled.blocks[index].parent;
    }
}

/// Refuses `until`, `retries` and `delay` on a task that has nowhere to retry them, naming the
/// first one written and the module.
fn refuse_retries(task: &PlayTask) -> anyhow::Result<()> {
    if let Some(kw) = [
        (!task.until.is_empty()).then_some("until"),
        task.retries.is_some().then_some("retries"),
        task.delay.is_some().then_some("delay"),
    ]
    .into_iter()
    .flatten()
    .next()
    {
        bail!(
            "task '{}': keyword '{kw}' is not supported yet on '{}'",
            task.name,
            task.module
        );
    }
    Ok(())
}

/// One task's arguments against the registry of the module it names.
///
/// Only an argument the reference **has** and this release does not act on is refused here. A
/// name neither engine has is left to the reference's own answer, which the agent produces at run
/// time with the reference's sentence and the reference's code: refusing it here would give the
/// same words and the same moment to a playbook waiting on this release and a playbook with a
/// typo in it, and those are different mistakes.
///
/// The internal keys need no exemption. `_raw_params` carries the command line of every free-form
/// task and `_uses_shell` carries the shell semantics, and the reference lists both among the
/// parameters it accepts, so both are in the registry with everything else.
///
/// A refusal the registry ties to one value fires on that value alone, the way `check_mode` is
/// refused for `true` and runs for `false`. A value this cannot read - a template, rendered per
/// host long after the pre-flight has run, and `{{ omit }}` among them - is left to run rather
/// than refused on a guess, because a guess here stops a playbook before it reaches a host.
fn check_arguments(task: &PlayTask) -> anyhow::Result<()> {
    let Some(spec) = volant_protocol::modules::native(&task.module) else {
        return Ok(());
    };
    for (key, value) in &task.args {
        let Some(ArgStatus::Refused(guard)) = spec
            .args
            .iter()
            .find(|a| a.name == key.as_str())
            .map(|a| a.status)
        else {
            continue;
        };
        if let Some(refused) = guard
            && volant_protocol::modules::arg_bool(value) != Some(refused)
        {
            continue;
        }
        let with = guard.map(|v| format!(" with '{v}'")).unwrap_or_default();
        bail!(
            "task '{}': argument '{key}' is not supported yet{with} on '{}'",
            task.name,
            task.module
        );
    }
    Ok(())
}

/// A `meta` task: the action has to be one the reference has, and one this release honours.
///
/// Exit 1 for an action nobody has, which is the reference's own code for it - measured, and
/// notably not the 4 a playbook it cannot load gets. Where the two differ is the moment: the
/// reference shows the `TASK [meta]` banner and fails there, this refuses before the first
/// connection, so nothing has run when the operator reads the message.
fn check_meta(task: &PlayTask) -> anyhow::Result<()> {
    let action = meta_action(task);
    match META_ACTIONS.iter().find(|(name, _)| *name == action) {
        Some((_, true)) => Ok(()),
        Some((name, false)) => bail!("task '{}': meta '{name}' is not supported yet", task.name),
        None => Err(Refusal::at(
            1,
            format!("invalid meta action requested: {action}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playbook::parse;
    use crate::stats::error_code;
    use rstest::rstest;

    /// A `pause` that would wait for an answer on a terminal is refused before anything connects
    /// when the step is sure to run, and left to the run when something may leave it out.
    #[test]
    fn a_pause_that_would_prompt_on_a_terminal_is_refused_only_when_sure_to_run() {
        let verdict_under = |tasks: &str, interactive: bool, run: Vec<String>| {
            let pb = parse(
                &format!("- hosts: all\n  gather_facts: false\n  tasks:\n{tasks}"),
                "x.yml",
            )
            .unwrap();
            let compiled = crate::compile::compile(
                &pb.plays[0],
                &crate::roles::RoleSearch::default(),
                &crate::compile::TagSelection::new(run, Vec::new()),
            )
            .unwrap();
            compiled
                .steps
                .iter()
                .try_for_each(|step| check_pause(&compiled, step, interactive))
                .map_err(|e| format!("{e:#}"))
        };
        let verdict =
            |tasks: &str, interactive: bool| verdict_under(tasks, interactive, Vec::new());
        for pause in ["{prompt: Go?}", "{}", "{prompt: Go?, seconds: 1}"] {
            let tasks = format!("    - name: Wait\n      pause: {pause}\n");
            assert_eq!(
                verdict(&tasks, true),
                Err(format!("task 'Wait': {PROMPT_REFUSED}")),
                "{pause}"
            );
            assert_eq!(verdict(&tasks, false), Ok(()), "{pause}");
        }
        for tasks in [
            "    - pause: {seconds: 1}\n",
            "    - pause: {minutes: 1}\n",
            "    - pause: {prompt: Go?}\n      when: confirm | default(false)\n",
            "    - pause: {prompt: Go?}\n      tags: [never]\n",
            "    - pause: {prompt: Go?}\n      loop: \"{{ pending | default([]) }}\"\n",
            "    - block:\n        - pause: {prompt: Go?}\n      when: confirm | default(false)\n",
            "    - block:\n        - debug: msg=hi\n      rescue:\n        - pause: {prompt: Go?}\n",
            "    - block:\n        - debug: msg=hi\n      rescue:\n        - block:\n            - pause: {prompt: Go?}\n",
        ] {
            assert_eq!(verdict(tasks, true), Ok(()), "{tasks}");
        }
        // A `never` task that `--tags never` selects is in the compiled steps and runs.
        assert!(
            verdict_under(
                "    - name: Wait\n      pause: {prompt: Go?}\n      tags: [never]\n",
                true,
                vec!["never".into()],
            )
            .is_err()
        );
    }

    fn refusal(text: &str) -> String {
        let pb = parse(text, "x.yml").expect("the loader accepts the whole grammar");
        let err = check(&pb).unwrap_err();
        assert_eq!(error_code(&err), 4, "{err:#}");
        format!("{err:#}")
    }

    /// A one-task play around `body`, for the rows that only differ in the task.
    fn task_play(body: &str) -> String {
        format!("- hosts: all\n  tasks:\n    - name: T\n{body}")
    }

    /// Everything the pre-flight refuses is refused at exit 4, naming what it refuses.
    #[rstest]
    #[case::async_(task_play("      command: echo hi\n      async: x\n"), &["keyword 'async' is not supported yet", "play 1", "'T'"])]
    #[case::diff(task_play("      command: echo hi\n      diff: x\n"), &["keyword 'diff' is not supported yet", "play 1", "'T'"])]
    #[case::poll(task_play("      command: echo hi\n      poll: x\n"), &["keyword 'poll' is not supported yet", "play 1", "'T'"])]
    #[case::throttle(task_play("      command: echo hi\n      throttle: x\n"), &["keyword 'throttle' is not supported yet", "play 1", "'T'"])]
    #[case::play_vars_prompt("- hosts: all\n  vars_prompt: 1\n  tasks:\n    - command: echo hi\n".to_string(), &["play keyword 'vars_prompt' is not supported yet"])]
    #[case::play_order("- hosts: all\n  order: 1\n  tasks:\n    - command: echo hi\n".to_string(), &["play keyword 'order' is not supported yet"])]
    #[case::parked_in_a_block_body(
        "- hosts: all\n  tasks:\n    - block:\n        - name: Deep\n          command: echo hi\n          throttle: probe\n".to_string(),
        &["task 'Deep': keyword 'throttle' is not supported yet"]
    )]
    #[case::parked_in_a_rescue(
        "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      rescue:\n        - name: Deep\n          command: echo hi\n          throttle: probe\n".to_string(),
        &["task 'Deep': keyword 'throttle' is not supported yet"]
    )]
    #[case::parked_in_an_always(
        "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      always:\n        - name: Deep\n          command: echo hi\n          throttle: probe\n".to_string(),
        &["task 'Deep': keyword 'throttle' is not supported yet"]
    )]
    #[case::until_on_include_tasks(task_play("      include_tasks: inc.yml\n      until: false\n"), &["keyword 'until' is not supported yet on 'include_tasks'"])]
    #[case::retries_on_include_tasks(task_play("      include_tasks: inc.yml\n      retries: 2\n"), &["keyword 'retries' is not supported yet on 'include_tasks'"])]
    #[case::delay_on_include_tasks(task_play("      include_tasks: inc.yml\n      delay: 1\n"), &["keyword 'delay' is not supported yet on 'include_tasks'"])]
    #[case::until_on_include_role(task_play("      include_role:\n        name: r\n      until: false\n"), &["keyword 'until' is not supported yet on 'include_role'"])]
    #[case::retries_on_include_role(task_play("      include_role:\n        name: r\n      retries: 2\n"), &["keyword 'retries' is not supported yet on 'include_role'"])]
    #[case::delay_on_include_role(task_play("      include_role:\n        name: r\n      delay: 1\n"), &["keyword 'delay' is not supported yet on 'include_role'"])]
    #[case::check_mode_on_a_task(task_play("      command: echo hi\n      check_mode: true\n"), &["'check_mode' is not supported yet with 'true'"])]
    #[case::check_mode_on_a_play("- hosts: all\n  check_mode: true\n  tasks:\n    - command: echo hi\n".to_string(), &["'check_mode' is not supported yet with 'true'"])]
    #[case::check_mode_on_a_block("- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      check_mode: true\n".to_string(), &["'check_mode' is not supported yet with 'true'"])]
    #[case::shell_true(task_play("      ansible.legacy.shell: /bin/echo $HOME\n      args:\n        expand_argument_vars: true\n"), &["argument 'expand_argument_vars' is not supported yet on 'ansible.legacy.shell'"])]
    #[case::shell_false(task_play("      ansible.legacy.shell: /bin/echo $HOME\n      args:\n        expand_argument_vars: false\n"), &["argument 'expand_argument_vars' is not supported yet on 'ansible.legacy.shell'"])]
    #[case::shell_no(task_play("      ansible.legacy.shell: /bin/echo $HOME\n      args:\n        expand_argument_vars: \"no\"\n"), &["argument 'expand_argument_vars' is not supported yet on 'ansible.legacy.shell'"])]
    #[case::strategy_free("- hosts: all\n  strategy: free\n  tasks:\n    - command: echo hi\n".to_string(), &["strategy 'free' is not supported yet"])]
    #[case::with_unvault(task_play("      debug: msg=hi\n      with_unvault: [x]\n"), &["keyword 'with_unvault' is not supported yet"])]
    #[case::action_plugin_assemble(task_play("      assemble: src=a dest=b\n"), &["module 'assemble' needs an action plugin"])]
    #[case::action_plugin_script(task_play("      script: src=a dest=b\n"), &["module 'script' needs an action plugin"])]
    #[case::action_plugin_builtin_script(task_play("      ansible.builtin.script: src=a dest=b\n"), &["module 'ansible.builtin.script' needs an action plugin"])]
    #[case::collection_splat(task_play("      ns.coll.mod: \"{{ d }}\"\n"), &["task 'T': module 'ns.coll.mod' takes its arguments from one template"])]
    #[case::builtin_splat(task_play("      ping: \"{{ d }}\"\n"), &["task 'T': module 'ping' takes its arguments from one template"])]
    #[case::builtin_no_collection_can_supply(task_play("      ansible.builtin.nosuch: x=1\n"), &["couldn't resolve module/action 'ansible.builtin.nosuch'"])]
    fn the_preflight_refuses_by_name(#[case] text: String, #[case] needles: &[&str]) {
        let got = refusal(&text);
        for needle in needles {
            assert!(got.contains(needle), "{needle:?} not in {got}");
        }
    }

    /// What the pre-flight lets through where the value or the module decides, or the controller
    /// answers.
    #[rstest]
    #[case::check_mode_false_on_a_task(task_play(
        "      command: echo hi\n      check_mode: false\n"
    ))]
    #[case::check_mode_false_on_a_play("- hosts: all\n  check_mode: false\n  tasks:\n    - command: echo hi\n".to_string())]
    #[case::retry_on_an_ordinary_task(task_play(
        "      command: echo hi\n      until: false\n      retries: 2\n      delay: 1\n"
    ))]
    #[case::retry_on_include_vars(task_play("      include_vars: v.yml\n      retries: 2\n"))]
    #[case::linear("- hosts: all\n  strategy: linear\n  tasks:\n    - command: echo hi\n".to_string())]
    #[case::unknown_argument_is_the_agents(task_play(
        "      command: /bin/true\n      args:\n        no_such_arg: 1\n"
    ))]
    #[case::arguments_this_release_honours(task_play(
        "      command: /bin/true\n      args:\n        chdir: /tmp\n        stdin_add_newline: false\n"
    ))]
    #[case::raw_takes_no_such_check(task_play(
        "      raw: /bin/true\n      args:\n        expand_argument_vars: true\n"
    ))]
    #[case::expansion_on(task_play(
        "      command: /bin/echo $HOME
      args:
        expand_argument_vars: true
"
    ))]
    #[case::expansion_off(task_play(
        "      command: /bin/echo $HOME\n      args:\n        expand_argument_vars: false\n"
    ))]
    #[case::shell_without_the_argument(task_play(
        "      shell: /bin/echo $HOME\n      args:\n        chdir: /tmp\n"
    ))]
    #[case::omitted_argument(task_play(
        "      command: /bin/echo $HOME\n      args:\n        expand_argument_vars: \"{{ omit }}\"\n"
    ))]
    #[case::builtin_run_through_a_payload(task_play("      lineinfile: path=/tmp/x\n"))]
    #[case::implemented_module(task_play("      command: echo hi\n"))]
    #[case::controller_side_module(task_play("      debug: msg=hi\n"))]
    #[case::name_left_to_the_controller(task_play("      nosuchmodule: x\n"))]
    #[case::builtin_stray_word(task_play("      ping: foo\n"))]
    #[case::retry_on_a_plugin(
        "- hosts: all\n  tasks:\n    - name: Copy k3s.yaml to second file\n      copy:\n        src: /etc/rancher/k3s/k3s.yaml\n        dest: /etc/rancher/k3s/k3s-copy.yaml\n        mode: \"0600\"\n        remote_src: true\n      retries: 3\n      register: k3s_server_copy_yaml\n    - name: Again\n      package: name=bash\n      until: false\n      delay: 1\n".to_string()
    )]
    #[case::static_with_lookups(
        "- hosts: all\n  tasks:\n    - name: Distribute K3s binary\n      copy:\n        src: \"{{ item }}\"\n        dest: /usr/local/bin/k3s\n      with_first_found:\n        - files:\n            - \"{{ airgap_dir }}/k3s\"\n          skip: true\n    - name: Distribute K3s images\n      copy:\n        src: \"{{ item }}\"\n        dest: /var/lib/rancher/k3s/agent/images/\n      with_fileglob:\n        - \"{{ airgap_dir }}/*.tar.gz\"\n    - name: Execute OS related tasks on the Raspberry Pi\n      include_tasks: \"{{ item }}\"\n      with_first_found:\n        - \"setup/{{ ansible_distribution }}.yml\"\n        - setup/default.yml\n".to_string()
    )]
    fn the_preflight_lets_through(#[case] text: String) {
        let pb = parse(&text, "x.yml").unwrap();
        check(&pb).unwrap_or_else(|e| panic!("{text}: {e:#}"));
    }

    /// The plugins and fact modules this release runs are admitted with their arguments read.
    #[rstest]
    fn the_modules_this_release_runs_are_admitted(
        #[values(
            "package",
            "service",
            "copy",
            "template",
            "unarchive",
            "fetch",
            "reboot",
            "ansible.builtin.package",
            "setup",
            "package_facts",
            "service_facts",
            "mount_facts",
            "getent"
        )]
        module: &str,
    ) {
        let pb = parse(
            &format!("- hosts: all\n  tasks:\n    - name: Now\n      {module}: name=bash\n"),
            "x.yml",
        )
        .unwrap();
        check(&pb).unwrap_or_else(|e| panic!("{module}: {e:#}"));
        let TaskOrBlock::Task(task) = &pb.plays[0].tasks[0] else {
            panic!("a task");
        };
        assert_eq!(task.args.get("name"), Some(&serde_json::json!("bash")));
        if [
            "setup",
            "package_facts",
            "service_facts",
            "mount_facts",
            "getent",
        ]
        .contains(&module)
        {
            assert!(crate::python::is_python_module(module), "{module}");
        }
    }

    /// A collection's module passes both passes and is collected for the controller to resolve; a
    /// stray word fails the task when it runs; the controller's answers are refused in its words.
    #[test]
    fn collection_modules_are_set_aside_and_the_controllers_answers_refused() {
        let text = "- hosts: all\n  gather_facts: false\n  tasks:\n    - name: Forward\n      ansible.posix.sysctl: name=net.ipv4.ip_forward value=1\n    - debug: msg=x\n  handlers:\n    - name: Reload\n      community.general.ufw: state=reloaded\n";
        let pb = parse(text, "x.yml").unwrap();
        check(&pb).expect("a collection's module waits for the controller's answer");
        let compiled = crate::compile::compile(
            &pb.plays[0],
            &crate::roles::RoleSearch::default(),
            &crate::compile::TagSelection::new(Vec::new(), Vec::new()),
        )
        .unwrap();
        check_steps(&compiled, true).expect("and so does the second pass");
        assert_eq!(
            collection_modules(&compiled),
            [
                ("Forward".to_string(), "ansible.posix.sysctl".to_string()),
                ("Reload".to_string(), "community.general.ufw".to_string()),
            ]
        );
        let stray = |module: &str| {
            let pb = parse(
                &format!("- hosts: all\n  tasks:\n    - name: Stray\n      {module}: a=1 stray\n"),
                "x.yml",
            )
            .unwrap();
            check(&pb).expect("set aside before it resolved");
            let compiled = crate::compile::compile(
                &pb.plays[0],
                &crate::roles::RoleSearch::default(),
                &crate::compile::TagSelection::new(Vec::new(), Vec::new()),
            )
            .unwrap();
            let facts = crate::python::ModuleFacts {
                module_fqn: "fqn".into(),
                profile: "legacy".into(),
                rlimit_nofile: 0,
                extensions: serde_json::Map::new(),
                core: false,
            };
            let union = crate::python::Union {
                hash: "ab".into(),
                zip_b64: "UEsDBA==".into(),
                modules: std::collections::BTreeMap::from([(module.to_string(), facts)]),
                refused: std::collections::BTreeMap::new(),
                natives: crate::python::Natives::default(),
            };
            check_built(&compiled, Some(&union)).map_err(|err| format!("{err:#}"))
        };
        for module in [
            "ns.coll.mod",
            "my_library_module",
            "ansible.windows.win_shell",
        ] {
            stray(module).expect("a stray word fails the task at run time");
        }

        // What the controller made of a collection's module, and what a built union holds,
        // refused in words the operator reads, all at the pre-flight's exit 4.
        use crate::python::Resolved;
        let verdict = |answer: Resolved| {
            check_resolved("T", "ns.coll.mod", &answer).map_err(|err| {
                assert_eq!(error_code(&err), 4, "{err:#}");
                format!("{err:#}")
            })
        };
        assert!(
            verdict(Resolved::Module {
                fqcn: "ns.coll.mod".into(),
                collection: Some(("ns.coll".into(), "1.0.0".into())),
            })
            .is_ok()
        );
        assert_eq!(
            verdict(Resolved::ActionPlugin {
                fqcn: "ns.coll.mod".into()
            }),
            Err("task 'T': module 'ns.coll.mod' needs an action plugin from its collection, which this release does not run".to_string())
        );
        let typo = "task 'T': couldn't resolve module/action 'ns.coll.mod'. This often indicates a misspelling, missing collection, or incorrect module path.";
        assert_eq!(
            verdict(Resolved::Missing { collection: None }),
            Err(typo.to_string())
        );
        assert_eq!(
            verdict(Resolved::Missing {
                collection: Some("ns.coll".into())
            }),
            Err(format!(
                "{typo} The collection 'ns.coll' is not installed on the controller: install it with ansible-galaxy collection install ns.coll"
            ))
        );

        // A collection's module an include brings in runs only if the union holds it.
        let pb = parse(
            "- hosts: all\n  gather_facts: false\n  tasks:\n    - name: Forward\n      ansible.posix.sysctl: name=x value=1\n",
            "x.yml",
        )
        .unwrap();
        let compiled = crate::compile::compile(
            &pb.plays[0],
            &crate::roles::RoleSearch::default(),
            &crate::compile::TagSelection::new(Vec::new(), Vec::new()),
        )
        .unwrap();
        let union = |keys: &[&str]| crate::python::Union {
            hash: "ab".into(),
            zip_b64: "UEsDBA==".into(),
            modules: keys
                .iter()
                .map(|k| {
                    (
                        k.to_string(),
                        crate::python::ModuleFacts {
                            module_fqn: format!("fqn.{k}"),
                            profile: "legacy".into(),
                            rlimit_nofile: 0,
                            extensions: serde_json::Map::new(),
                            core: true,
                        },
                    )
                })
                .collect(),
            refused: std::collections::BTreeMap::new(),
            natives: crate::python::Natives::default(),
        };
        check_built(&compiled, Some(&union(&["ansible.posix.sysctl"])))
            .expect("the union holds it");
        for refused in [None, Some(union(&["sysctl", "ping"]))] {
            let err = check_built(&compiled, refused.as_ref()).unwrap_err();
            let text = format!("{err:#}");
            assert!(
                text.contains("task 'Forward': module 'ansible.posix.sysctl' was not resolved"),
                "{text}"
            );
        }
        let mut set_aside = union(&["ping"]);
        set_aside
            .refused
            .insert("ansible.posix.sysctl".into(), "it is gone.".into());
        assert_eq!(
            format!(
                "{:#}",
                check_built(&compiled, Some(&set_aside)).unwrap_err()
            ),
            "task 'Forward': it is gone."
        );
    }

    /// One probe playbook per `Preflight` row of the keyword tables, built from the row's own
    /// name. The value is never read - a parked keyword is kept, not parsed - so one
    /// placeholder serves every row, and a keyword that starts being read will say so by
    /// failing to load.
    fn preflight_probes() -> Vec<(&'static str, String)> {
        use crate::keywords::{
            BLOCK_KEYWORDS, HANDLER_KEYWORDS, LOOP_CONTROL_KEYWORDS, PLAY_KEYWORDS, Support,
            TASK_KEYWORDS,
        };
        let task = |kw: &str| {
            format!(
                "- hosts: all\n  tasks:\n    - name: Probe task\n      command: echo hi\n      {kw}: probe\n"
            )
        };
        let parked = |table: &'static [crate::keywords::Keyword]| {
            table
                .iter()
                .filter(|k| k.support == Support::Preflight)
                .map(|k| k.name)
        };
        let mut probes: Vec<(&'static str, String)> =
            parked(TASK_KEYWORDS).map(|kw| (kw, task(kw))).collect();
        probes.extend(parked(BLOCK_KEYWORDS).map(|kw| {
            // A parked section keyword takes a task list; every other value is kept and never
            // parsed, so the one placeholder serves the rest.
            let value = if crate::keywords::BLOCK_SECTIONS.contains(&kw) {
                ":\n        - name: Deep\n          command: echo hi"
            } else {
                ": probe"
            };
            (
                kw,
                format!(
                    "- hosts: all\n  tasks:\n    - block:\n        - name: Probe task\n          command: echo hi\n      {kw}{value}\n"
                ),
            )
        }));
        probes.extend(parked(PLAY_KEYWORDS).map(|kw| {
            (
                kw,
                format!(
                    "- hosts: all\n  {kw}: probe\n  tasks:\n    - name: Probe task\n      command: echo hi\n"
                ),
            )
        }));
        probes.extend(parked(LOOP_CONTROL_KEYWORDS).map(|kw| {
            (
                kw,
                format!(
                    "- hosts: all\n  tasks:\n    - name: Probe task\n      debug:\n        msg: probe\n      loop: [alpha]\n      loop_control:\n        {kw}: probe\n"
                ),
            )
        }));
        probes.extend(parked(HANDLER_KEYWORDS).map(|kw| {
            (
                kw,
                format!(
                    "- hosts: all\n  handlers:\n    - name: Probe task\n      command: echo hi\n      {kw}: probe\n  tasks:\n    - command: echo hi\n"
                ),
            )
        }));
        probes
    }

    /// Every keyword the tables mark `Preflight` stops the run, naming itself, and every `meta`
    /// action is honoured or refused by its own name.
    #[test]
    fn every_table_row_is_refused_by_its_own_name() {
        let probes = preflight_probes();
        assert_eq!(
            probes.len(),
            18,
            "the tables carry the whole grammar; this count is the record"
        );
        for (kw, body) in &probes {
            let text = refusal(body);
            assert!(
                text.contains(&format!("keyword '{kw}' is not supported yet")),
                "{kw} must name itself: {text}"
            );
        }
        let play =
            |action: &str| format!("- hosts: all\n  tasks:\n    - name: M\n      meta: {action}\n");
        for (action, supported) in META_ACTIONS {
            let pb = parse(&play(action), "x.yml").expect("the loader takes every action");
            let checked = check(&pb);
            if *supported {
                assert!(checked.is_ok(), "{action} runs");
            } else {
                let err = checked.unwrap_err();
                assert_eq!(error_code(&err), 4, "{err:#}");
                assert!(
                    format!("{err:#}").contains(&format!("meta '{action}' is not supported yet")),
                    "{err:#}"
                );
            }
        }
        // Measured on ansible-core 2.19.12: `invalid meta action requested: nosuchaction`,
        // exit 1 - not the 4 an unloadable playbook gets.
        let pb = parse(&play("nosuchaction"), "x.yml").unwrap();
        let err = check(&pb).unwrap_err();
        assert_eq!(error_code(&err), 1, "{err:#}");
        assert!(
            format!("{err:#}").contains("invalid meta action requested: nosuchaction"),
            "{err:#}"
        );
    }
}
