// SPDX-License-Identifier: GPL-3.0-or-later
//! The refusals of single keywords and shapes: pause, retries, async, arguments and meta.

use anyhow::bail;
use volant_protocol::modules::{ArgStatus, import_module, include_module, short_name};

use super::CODE;
use crate::action_plugins;
use crate::compile::{META_ACTIONS, meta_action};
use crate::playbook::{PlayTask, is_meta};
use crate::stats::Refusal;

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
pub(super) fn check_pause(
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
pub(super) fn runs_whatever_happens(
    compiled: &crate::compile::Compiled,
    step: &crate::compile::Step,
) -> bool {
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
pub(super) fn refuse_retries(task: &PlayTask) -> anyhow::Result<()> {
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
pub(super) fn refuse_async(task: &PlayTask) -> anyhow::Result<()> {
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
pub(super) fn check_arguments(task: &PlayTask) -> anyhow::Result<()> {
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
pub(super) fn check_meta(task: &PlayTask) -> anyhow::Result<()> {
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
pub(super) fn check_end_role(step: &crate::compile::Step) -> anyhow::Result<()> {
    if is_meta(&step.task) && meta_action(&step.task) == "end_role" && step.role.is_none() {
        return Err(Refusal::at(
            CODE,
            "Cannot execute 'end_role' from outside of a role",
        ));
    }
    Ok(())
}
