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
    if let Some(kw) = play.unsupported.first() {
        bail!("play keyword '{kw}' is not supported yet");
    }
    if let Some(strategy) = &play.strategy
        && !STRATEGIES.contains(&strategy.as_str())
    {
        bail!("strategy '{strategy}' is not supported yet");
    }
    // `max_fail_percentage` decides after each step whether the batch goes on, which hosts that
    // do not wait for each other have no step to decide at.
    if let Some(strategy) = play.strategy.as_deref()
        && matches!(strategy, "free" | "host_pinned")
        && play.max_fail_percentage.is_some()
    {
        bail!("strategy '{strategy}' with 'max_fail_percentage' is not supported yet");
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
                if let Some(kw) = block.keywords.unsupported.first() {
                    bail!(
                        "block '{}': keyword '{kw}' is not supported yet",
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
    refuse_async(task)?;
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
/// Volant never installs a collection. A collection's action plugin is not refused: it runs in the
/// Python helper (see `python::types::note_action_plugins`). A missing collection is named, with the command that
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
        // A collection's action plugin runs in the Python helper, as ansible-core's own, unless
        // it needs Windows or a network device's connection.
        Resolved::ActionPlugin { fqcn, .. } => (action_plugins::waits_for_later(module)
            || action_plugins::waits_for_later(fqcn))
        .then(|| {
            format!(
                "module '{fqcn}' needs an action plugin from its collection, which this release does not run"
            )
        }),
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
/// module it could build, or an action plugin the helper runs. One that is not was missing, or sits
/// in a file nothing read before the run; the include fails for the host that reached it, before
/// any of the file's tasks run, like any other refusal of an included file.
pub(crate) fn check_built(
    expanded: &crate::compile::Compiled,
    union: Option<&crate::python::Union>,
) -> anyhow::Result<()> {
    check_collection_actions(expanded)?;
    for task in set_aside(expanded).filter(|task| action_plugins::kind(&task.module).is_none()) {
        let (name, module) = (&task.name, &task.module);
        if !union.is_some_and(|u| u.modules.contains_key(crate::python::payload_key(module))) {
            // What the controller made of a name it set aside, as the pre-flight says it for one
            // a play names.
            if let Some(why) = union.and_then(|u| u.refused.get(module)) {
                bail!("task '{name}': {why}");
            }
            bail!(
                "task '{name}': module '{module}' was not resolved to a module before the run, so no payload holds it: its collection is not installed on the controller, no module path holds it, or it is named only in a file nothing read before the first connection"
            );
        }
    }
    Ok(())
}

/// `async` on a collection's action plugin, which the first pass could not see for a plugin until
/// the controller had resolved the name: refused as it is on the builtin ones.
pub(crate) fn check_collection_actions(compiled: &crate::compile::Compiled) -> anyhow::Result<()> {
    set_aside(compiled).try_for_each(refuse_async)
}

/// Why a `pause` that would wait for an answer is refused on a terminal.
pub(crate) const PROMPT_REFUSED: &str = "pause cannot wait for a prompt and a duration together on a terminal because Volant would have to read the answer while the time runs. Give the pause a prompt or seconds or minutes, not both, or redirect standard input.";

/// A `pause` with a `prompt` and a duration, refused before the first connection when standard
/// input is a terminal and the step runs whatever happens. The shape is written in the playbook
/// (a `prompt` next to `seconds` or `minutes`), and the terminal is this process's; refused
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
    if has("prompt") && (has("seconds") || has("minutes")) {
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

/// Refuses `async` on a task that starts no module on the host: the controller's own modules,
/// the action plugins this release runs on the controller, the statements, and the two agent
/// modules the reference refuses it on (`raw` and `async_status`, whose action plugins say
/// `does not support async`). What is left is what the agent runs as a job of its own.
fn refuse_async(task: &PlayTask) -> anyhow::Result<()> {
    if task.async_val == 0 {
        return Ok(());
    }
    let module = &task.module;
    let refused = volant_protocol::modules::local(module).is_some()
        || import_module(module).is_some()
        || include_module(module).is_some()
        || is_meta(task)
        || action_plugins::kind(module).is_some()
        || volant_protocol::modules::native(module)
            .is_some_and(|spec| matches!(spec.name, "raw" | "async_status"));
    if refused {
        bail!(
            "task '{}': keyword 'async' is not supported yet on '{module}'",
            task.name
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
/// A refusal the registry ties to one value fires on that value alone. A value this cannot
/// read - a template, rendered per
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

/// A `meta` task: the action has to be one the reference has.
///
/// Exit 1 for an action nobody has, which is the reference's own code for it - measured, and
/// notably not the 4 a playbook it cannot load gets. Where the two differ is the moment: the
/// reference shows the `TASK [meta]` banner and fails there, this refuses before the first
/// connection, so nothing has run when the operator reads the message.
fn check_meta(task: &PlayTask) -> anyhow::Result<()> {
    let action = meta_action(task);
    if META_ACTIONS.contains(&action) {
        Ok(())
    } else {
        Err(Refusal::at(
            1,
            format!("invalid meta action requested: {action}"),
        ))
    }
}

/// `end_role` written where no role is: the reference stops loading the playbook, exit 4. Only the
/// compiled steps know which role a task belongs to, so this is asked of them and not of the
/// play's own lists.
fn check_end_role(step: &crate::compile::Step) -> anyhow::Result<()> {
    if is_meta(&step.task) && meta_action(&step.task) == "end_role" && step.role.is_none() {
        return Err(Refusal::at(
            CODE,
            "Cannot execute 'end_role' from outside of a role",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
