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
use volant_protocol::modules::{ArgStatus, import_module, include_module, is_builtin, is_known};

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
pub(crate) fn check_steps(compiled: &crate::compile::Compiled) -> anyhow::Result<()> {
    for step in &compiled.steps {
        // A flush point the compiler put in itself carries no task at all, so there is no module
        // and no `meta` action to judge. No handler step exists yet - the coordinator splices
        // those in as the run reaches each flush - so the handlers themselves are checked below,
        // off `compiled.handlers`. Everything else goes through, `meta` included: a role's
        // `meta: end_play` has to be refused here, since the first pass never saw that file.
        if !matches!(step.kind, crate::compile::StepKind::Flush { .. }) {
            check_task(&step.task).map_err(|e| Refusal::or(CODE, e))?;
        }
        check_notify(compiled, &step.task)?;
    }
    for handler in &compiled.handlers {
        check_task(&handler.task).map_err(|e| Refusal::or(CODE, e))?;
        // A handler notifying another handler is ordinary - measured, the second one runs in the
        // same flush when it is defined behind the first - so its names are resolved here too.
        check_notify(compiled, &handler.task)?;
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
/// [`check_notify`] is deliberately left out. The expansion is compiled on its own, so the handler
/// list it carries is not the play's, and a task notifying a handler the play defines would be
/// refused here for a name that resolves perfectly once grafted.
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

/// Every `notify` on a step or a handler names a handler this play has.
///
/// Measured on ansible-core 2.19.12: a name nothing answers to stops the run with this sentence
/// and exit **1**, after the notifying task's banner and with no recap. This refuses before the
/// first connection instead, so nothing has run when the operator reads it - the same code, one
/// moment earlier, and the divergence is deliberate.
///
/// A templated name is the one shape this cannot resolve here, and it is refused rather than left
/// to a run. Measured: the reference renders it and finds the handler, so this is a divergence -
/// but resolving it would mean rendering the name per host and per loop item, and a name that
/// then answers to nothing would have to fail a task that has already printed its result line.
/// Refusing it by its own name is the smaller thing to be wrong about.
fn check_notify(c: &crate::compile::Compiled, task: &PlayTask) -> anyhow::Result<()> {
    for name in &task.notify {
        if crate::template::Templar::is_template(name) {
            return Err(Refusal::at(
                CODE,
                format!(
                    "task '{}': a templated 'notify' is not supported yet",
                    task.name
                ),
            ));
        }
        if crate::compile::resolve_notify(c, name).is_empty() {
            return Err(Refusal::at(
                1,
                format!(
                    "The requested handler '{name}' was not found in either the main handlers list nor in the listening handlers list"
                ),
            ));
        }
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
        return Ok(());
    }
    // Before the two sentences below, because a module the reference runs through an action
    // plugin is not a module waiting on this release to write it: the behaviour a playbook asks
    // for lives in the plugin, on the controller, and shipping the module alone would run
    // something else. `package` picks the host's package manager, `template` renders before the
    // task is sent, and neither is what the payload holds.
    if action_plugins::is_action_backed(&task.module) {
        bail!(
            "task '{}': module '{}' needs an action plugin, which this release does not run yet",
            task.name,
            task.module
        );
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
            return check_arguments(task);
        }
        bail!(
            "task '{}': couldn't resolve module/action '{}'. This often indicates a misspelling, missing collection, or incorrect module path.",
            task.name,
            task.module
        );
    }
    check_arguments(task)
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

    fn refusal(text: &str) -> String {
        let pb = parse(text, "x.yml").expect("the loader accepts the whole grammar");
        let err = check(&pb).unwrap_err();
        assert_eq!(error_code(&err), 4, "{err:#}");
        format!("{err:#}")
    }

    /// The three module states, each in its own words. What would make this red: the builtin
    /// arm answering with the typo sentence or the other way round, which is what a collapsed
    /// `is_builtin` produces.
    ///
    /// A builtin this release does not run itself is no longer refused at all: it has a path now,
    /// through the payload the controller builds for it. The three states are "this release runs
    /// it", "the reference runs it through an action plugin" and "nobody has that name".
    #[test]
    fn the_three_module_states_are_told_apart() {
        let pb = parse(
            "- hosts: all\n  tasks:\n    - name: Later\n      lineinfile: path=/tmp/x\n",
            "x.yml",
        )
        .unwrap();
        assert!(
            check(&pb).is_ok(),
            "a builtin this release does not run itself has a payload path now"
        );
        let text = refusal("- hosts: all\n  tasks:\n    - name: Later\n      nosuchmodule: x\n");
        assert!(
            text.contains("couldn't resolve module/action 'nosuchmodule'"),
            "{text}"
        );
        assert!(
            text.contains("This often indicates a misspelling"),
            "{text}"
        );
        let pb = parse("- hosts: all\n  tasks:\n    - command: echo hi\n", "x.yml").unwrap();
        assert!(check(&pb).is_ok(), "an implemented module passes");
    }

    /// A module whose only product is facts reaches a host now, and the pre-flight reads that
    /// from `python::is_python_module` rather than deciding it again.
    ///
    /// They were held back while nothing merged a result's `ansible_facts` into the variable
    /// store: `setup` would have reported `ok` on every host and left the next task reading
    /// `ansible_facts.*` undefined - a run the pre-flight accepted, broken halfway through.
    ///
    /// What would make this red: the two expressions drifting. A name `is_python_module` holds
    /// back and this arm still admits fails per host for want of a payload, which is a startup
    /// refusal turned into a failure on every host; a name this arm refuses and
    /// `is_python_module` admits is a module the run builds a payload nobody asks for.
    #[test]
    fn a_module_that_only_gathers_facts_reaches_its_host() {
        for module in [
            "setup",
            "package_facts",
            "service_facts",
            "mount_facts",
            "getent",
        ] {
            assert!(
                crate::python::is_python_module(module),
                "{module} runs through the payload path now that its facts are merged"
            );
            let pb = parse(
                &format!("- hosts: all\n  tasks:\n    - name: Later\n      {module}: a=b\n"),
                "x.yml",
            )
            .unwrap();
            assert!(check(&pb).is_ok(), "{module} is admitted by the pre-flight");
        }
    }

    /// A module the reference runs through an action plugin is refused by that name, before the
    /// first connection, and not as a module this release has merely not written yet.
    ///
    /// What would make this red: the arm placed after `is_builtin`, which sends `package`
    /// through `is_python_module` - false, because the plugin backs it - and so answers it with
    /// the "not available in this release" sentence the arm below prints, losing the one word
    /// that tells the operator where the behaviour lives. The plugin picks the host's package
    /// manager, and the module alone does not.
    #[test]
    fn a_module_backed_by_an_action_plugin_is_refused_by_that_name() {
        let text = refusal("- hosts: all\n  tasks:\n    - name: Later\n      package: name=bash\n");
        assert!(
            text.contains("module 'package' needs an action plugin"),
            "{text}"
        );
        let text = refusal(
            "- hosts: all\n  tasks:\n    - name: Later\n      ansible.builtin.template: src=a dest=b\n",
        );
        assert!(
            text.contains("module 'ansible.builtin.template' needs an action plugin"),
            "{text}"
        );
        let pb = parse("- hosts: all\n  tasks:\n    - debug: msg=hi\n", "x.yml").unwrap();
        assert!(check(&pb).is_ok(), "a controller-side module still runs");
    }

    /// `until`, `retries` and `delay` are refused on the two dynamic statements, and still run on
    /// an ordinary task.
    ///
    /// They cannot do anything on a statement this engine expands: the step loop resolves an
    /// include and splices behind it before `prepare`, which is where the retry plan is built. The
    /// reference refuses them too - measured on ansible-core 2.19.12,
    /// `'until' is not a valid attribute for a TaskInclude` at exit 4, and the same sentence
    /// naming `IncludeRole`, because `VALID_INCLUDE_KEYWORDS` holds sixteen names and none of
    /// these three.
    ///
    /// `include_vars` is not one of them: it is an ordinary controller-side module on both sides
    /// and both retry it - measured, two `FAILED - RETRYING` lines and `attempts: 2` each.
    ///
    /// What would make this red: any of the three accepted on a statement that expands, which is
    /// a retry the operator wrote, the engine dropped and the run never attempted; the refusal
    /// widened to ordinary tasks, which refuses the playbooks task 7 measured; or it widened to
    /// `include_vars`, which refuses a retry both engines perform.
    #[test]
    fn a_retry_on_a_dynamic_statement_is_refused_by_name() {
        for (kw, value) in [("until", "false"), ("retries", "2"), ("delay", "1")] {
            let text = refusal(&format!(
                "- hosts: all\n  tasks:\n    - name: T\n      include_tasks: inc.yml\n      {kw}: {value}\n"
            ));
            assert!(
                text.contains(&format!(
                    "keyword '{kw}' is not supported yet on 'include_tasks'"
                )),
                "{text}"
            );
            let text = refusal(&format!(
                "- hosts: all\n  tasks:\n    - name: T\n      include_role:\n        name: r\n      {kw}: {value}\n"
            ));
            assert!(
                text.contains(&format!(
                    "keyword '{kw}' is not supported yet on 'include_role'"
                )),
                "{text}"
            );
        }
        let pb = parse(
            "- hosts: all\n  tasks:\n    - name: T\n      command: echo hi\n      until: false\n      retries: 2\n      delay: 1\n",
            "x.yml",
        )
        .unwrap();
        assert!(check(&pb).is_ok(), "an ordinary task still retries");
        let pb = parse(
            "- hosts: all\n  tasks:\n    - name: T\n      include_vars: v.yml\n      retries: 2\n",
            "x.yml",
        )
        .unwrap();
        assert!(check(&pb).is_ok(), "and so does include_vars");
    }

    /// Every keyword the loader parked is refused by its own name, and the play it belongs to
    /// is named too. What would make this red: a keyword loaded into `unsupported` and then
    /// dropped here, which is exactly the silent skip this module exists to stop.
    #[test]
    fn a_parked_keyword_is_refused_by_name() {
        for kw in ["async", "diff", "poll", "remote_user", "throttle"] {
            let text = refusal(&format!(
                "- hosts: all\n  tasks:\n    - name: T\n      command: echo hi\n      {kw}: x\n"
            ));
            assert!(
                text.contains(&format!("keyword '{kw}' is not supported yet")),
                "{text}"
            );
            assert!(text.contains("play 1") && text.contains("'T'"), "{text}");
        }
        for kw in [
            "any_errors_fatal",
            "vars_prompt",
            "max_fail_percentage",
            "order",
        ] {
            let text = refusal(&format!(
                "- hosts: all\n  {kw}: 1\n  tasks:\n    - command: echo hi\n"
            ));
            assert!(
                text.contains(&format!("play keyword '{kw}' is not supported yet")),
                "{text}"
            );
        }
    }

    /// Every section of a block is walked. A keyword this release cannot execute has to be
    /// refused wherever it was written; one sitting inside a block would otherwise only stop
    /// the run once the block was reached, with half the playbook applied.
    ///
    #[test]
    fn a_keyword_parked_inside_any_section_of_a_block_is_refused() {
        let parked = "- name: Deep\n          command: echo hi\n          throttle: probe";
        for (section, body) in [
            ("block", format!("- block:\n        {parked}\n")),
            (
                "rescue",
                format!("- block:\n        - command: echo hi\n      rescue:\n        {parked}\n"),
            ),
            (
                "always",
                format!("- block:\n        - command: echo hi\n      always:\n        {parked}\n"),
            ),
        ] {
            let text = refusal(&format!("- hosts: all\n  tasks:\n    {body}"));
            assert!(
                text.contains("task 'Deep': keyword 'throttle' is not supported yet"),
                "{section}: {text}"
            );
        }
    }

    /// `meta` asks the engine for something rather than naming a module, so it is refused by the
    /// action it asked for and never for a module nobody wrote.
    ///
    /// What would make this red: `meta` falling through to the module check, which would tell
    /// the operator that a module they never named is missing; or an action this release cannot
    /// honour running as a no-op, which is the silent skip the pre-flight exists to stop.
    #[test]
    fn a_meta_action_is_honoured_or_refused_by_its_own_name() {
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

    /// One probe playbook per `Preflight` row of the three tables, built from the row's own
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
        // A block has a grammar of its own, so it has a table of its own and the same rule one
        // level in: a keyword the reference lets a block carry and this release cannot honour
        // is refused by its name rather than inherited by every task underneath and ignored.
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
        // A handler has a grammar of its own too, one name wide today. It is walked here so that
        // a row added to it is probed by the same rule as every other table's.
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

    /// Every keyword the tables mark `Preflight` stops the run, before the first connection,
    /// naming itself while doing it. Adding a `Preflight` row to any of the three tables adds a
    /// case here on its own, so the gap cannot be opened silently.
    ///
    /// The count is pinned rather than bounded: it is the number this release's record carries,
    /// and a floor let a wrong one stand once already. A table that grows moves it by hand, in
    /// the same change.
    ///
    /// What would make this red: a keyword the loader accepts and the pre-flight forgets, which
    /// would run the playbook without it and report success; or a refusal that stops naming the
    /// keyword, leaving the operator to guess which line to fix. That nothing runs before the
    /// refusal is the process fixture's half of the proof, in `playbook_cli.rs`.
    #[test]
    fn every_preflight_keyword_is_refused_by_its_own_name() {
        let probes = preflight_probes();
        assert_eq!(
            probes.len(),
            77,
            "the tables carry the whole grammar; this count is the record"
        );
        for (kw, body) in &probes {
            let text = refusal(body);
            assert!(
                text.contains(&format!("keyword '{kw}' is not supported yet")),
                "{kw} must name itself: {text}"
            );
        }
    }

    /// `check_mode` is the one `Runs` row whose value decides: `false` is what this release
    /// does, `true` is refused by name wherever it is written.
    ///
    /// What would make this red: `check_mode: true` accepted, which would run for real every
    /// task the operator asked to have only described; or `check_mode: false` refused, which
    /// would refuse a playbook that asked for exactly what this engine does.
    #[test]
    fn check_mode_runs_for_false_and_is_refused_for_true() {
        for body in [
            "- hosts: all\n  tasks:\n    - name: T\n      command: echo hi\n      check_mode: true\n",
            "- hosts: all\n  check_mode: true\n  tasks:\n    - command: echo hi\n",
            "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      check_mode: true\n",
        ] {
            let text = refusal(body);
            assert!(
                text.contains("'check_mode' is not supported yet with 'true'"),
                "{text}"
            );
        }
        for body in [
            "- hosts: all\n  tasks:\n    - name: T\n      command: echo hi\n      check_mode: false\n",
            "- hosts: all\n  check_mode: false\n  tasks:\n    - command: echo hi\n",
        ] {
            let pb = parse(body, "x.yml").unwrap();
            assert!(check(&pb).is_ok(), "check_mode: false is what we do");
        }
    }

    /// An argument the reference has and this release does not act on is refused by its own name,
    /// on every module whose registry carries it, and an argument neither engine has is left
    /// alone here so the agent can answer it with the reference's words.
    ///
    /// What would make this red: the refusal widened to every unknown name, which gives a typo
    /// this engine's message and this engine's code where the reference has its own; the refusal
    /// dropped, which lets the task reach an agent that ignores the argument and reports success;
    /// or `_raw_params` refused, which refuses every free-form task there is.
    ///
    /// The value decides where the reference leaves a value to decide, and `expand_argument_vars`
    /// on `command` is the one argument where it does. `false` asks for the expansion this
    /// release does not do, so the task runs; `true` asks for the one it cannot do, so the task
    /// is refused. Reading the name alone refused a playbook the two engines agreed on, and
    /// `"yes"` is here because the reference reads that spelling too.
    ///
    /// `shell` is the other half of the same argument and answers differently, because the
    /// reference does: measured on ansible-core 2.19.12, a `shell` task carrying the argument at
    /// all fails with `Unsupported parameters for (shell) module: expand_argument_vars`, whatever
    /// the value. So the name is refused there for every value, which is a playbook the reference
    /// refuses too, told sooner.
    #[test]
    fn a_module_argument_this_release_does_not_honour_is_refused_by_name() {
        for value in ["true", "\"yes\"", "1"] {
            let text = refusal(&format!(
                "- hosts: all\n  tasks:\n    - name: T\n      command: /bin/echo $HOME\n      args:\n        expand_argument_vars: {value}\n"
            ));
            assert!(
                text.contains(
                    "argument 'expand_argument_vars' is not supported yet with 'true' on 'command'"
                ),
                "{value}: {text}"
            );
        }
        for value in ["true", "false", "\"no\""] {
            let text = refusal(&format!(
                "- hosts: all\n  tasks:\n    - name: T\n      ansible.legacy.shell: /bin/echo $HOME\n      args:\n        expand_argument_vars: {value}\n"
            ));
            assert!(
                text.contains(
                    "argument 'expand_argument_vars' is not supported yet on 'ansible.legacy.shell'"
                ),
                "{value}: {text}"
            );
        }
        for body in [
            "- hosts: all\n  tasks:\n    - name: T\n      command: /bin/true\n      args:\n        no_such_arg: 1\n",
            "- hosts: all\n  tasks:\n    - name: T\n      command: /bin/true\n      args:\n        chdir: /tmp\n        stdin_add_newline: false\n",
            "- hosts: all\n  tasks:\n    - name: T\n      raw: /bin/true\n      args:\n        expand_argument_vars: true\n",
            "- hosts: all\n  tasks:\n    - name: T\n      command: /bin/echo $HOME\n      args:\n        expand_argument_vars: false\n",
            "- hosts: all\n  tasks:\n    - name: T\n      shell: /bin/echo $HOME\n      args:\n        chdir: /tmp\n",
            "- hosts: all\n  tasks:\n    - name: T\n      command: /bin/echo $HOME\n      args:\n        expand_argument_vars: \"{{ omit }}\"\n",
        ] {
            let pb = parse(body, "x.yml").unwrap();
            assert!(check(&pb).is_ok(), "{body}");
        }
    }

    #[test]
    fn only_the_linear_strategy_runs() {
        let text = refusal("- hosts: all\n  strategy: free\n  tasks:\n    - command: echo hi\n");
        assert!(
            text.contains("strategy 'free' is not supported yet"),
            "{text}"
        );
        let pb = parse(
            "- hosts: all\n  strategy: linear\n  tasks:\n    - command: echo hi\n",
            "x.yml",
        )
        .unwrap();
        assert!(check(&pb).is_ok(), "linear is what the engine does");
    }
}
