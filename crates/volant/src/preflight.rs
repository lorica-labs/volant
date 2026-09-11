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

use anyhow::bail;
use volant_protocol::modules::{import_module, is_builtin, is_known};

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
        check_task(&step.task).map_err(|e| Refusal::or(CODE, e))?;
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
    if !is_known(&task.module) {
        // Two different messages for two different situations. A module ansible-core ships and
        // this release has not written yet will arrive; a name no collection has never will,
        // and the operator has a typo to fix. The reference's own sentence carries the second.
        if is_builtin(&task.module) {
            bail!(
                "task '{}': module '{}' is not available in this release",
                task.name,
                task.module
            );
        }
        bail!(
            "task '{}': couldn't resolve module/action '{}'. This often indicates a misspelling, missing collection, or incorrect module path.",
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
    #[test]
    fn the_three_module_states_are_told_apart() {
        let text =
            refusal("- hosts: all\n  tasks:\n    - name: Later\n      lineinfile: path=/tmp/x\n");
        assert!(
            text.contains("module 'lineinfile' is not available in this release"),
            "{text}"
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

    /// Every keyword the loader parked is refused by its own name, and the play it belongs to
    /// is named too. What would make this red: a keyword loaded into `unsupported` and then
    /// dropped here, which is exactly the silent skip this module exists to stop.
    #[test]
    fn a_parked_keyword_is_refused_by_name() {
        for kw in ["until", "notify", "no_log", "tags", "environment"] {
            let text = refusal(&format!(
                "- hosts: all\n  tasks:\n    - name: T\n      command: echo hi\n      {kw}: x\n"
            ));
            assert!(
                text.contains(&format!("keyword '{kw}' is not supported yet")),
                "{text}"
            );
            assert!(text.contains("play 1") && text.contains("'T'"), "{text}");
        }
        for kw in ["serial", "vars_prompt", "handlers", "order"] {
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
    /// `rescue` is not among the sections walked for a task here because the section itself is
    /// parked: nothing enters a rescue yet, so a block carrying one is refused for the section
    /// rather than run without the recovery it describes.
    #[test]
    fn a_keyword_parked_inside_any_section_of_a_block_is_refused() {
        let parked = "- name: Deep\n          command: echo hi\n          no_log: probe";
        for (section, body) in [
            ("block", format!("- block:\n        {parked}\n")),
            (
                "always",
                format!("- block:\n        - command: echo hi\n      always:\n        {parked}\n"),
            ),
        ] {
            let text = refusal(&format!("- hosts: all\n  tasks:\n    {body}"));
            assert!(
                text.contains("task 'Deep': keyword 'no_log' is not supported yet"),
                "{section}: {text}"
            );
        }
        let text = refusal(
            "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      rescue:\n        - command: echo sorry\n",
        );
        assert!(
            text.contains("keyword 'rescue' is not supported yet"),
            "{text}"
        );
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
        for (action, supported) in crate::compile::META_ACTIONS {
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
            BLOCK_KEYWORDS, LOOP_CONTROL_KEYWORDS, PLAY_KEYWORDS, Support, TASK_KEYWORDS,
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
            105,
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
