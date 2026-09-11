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
use volant_protocol::modules::{is_builtin, is_known};

use crate::playbook::{Play, PlayTask, Playbook};
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
    for task in &play.tasks {
        check_task(task)?;
    }
    Ok(())
}

/// One task, checked on its own so the compiler can call it per step once blocks and roles put
/// tasks somewhere other than a play's own list.
pub fn check_task(task: &PlayTask) -> anyhow::Result<()> {
    if let Some(kw) = task.unsupported.first() {
        bail!("task '{}': keyword '{kw}' is not supported yet", task.name);
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
        for kw in ["serial", "roles", "handlers", "order"] {
            let text = refusal(&format!(
                "- hosts: all\n  {kw}: 1\n  tasks:\n    - command: echo hi\n"
            ));
            assert!(
                text.contains(&format!("play keyword '{kw}' is not supported yet")),
                "{text}"
            );
        }
    }

    /// A construct with no module of its own - a block - is refused for what it is rather than
    /// for the module it never named.
    #[test]
    fn a_block_is_refused_as_a_block_and_not_as_a_missing_module() {
        let text = refusal(
            "- hosts: all\n  tasks:\n    - name: Grouped\n      block:\n        - command: echo hi\n",
        );
        assert!(
            text.contains("keyword 'block' is not supported yet"),
            "{text}"
        );
        assert!(!text.contains("no module given"), "{text}");
    }

    /// One probe playbook per `Preflight` row of the three tables, built from the row's own
    /// name. The value is never read - a parked keyword is kept, not parsed - so one
    /// placeholder serves every row, and a keyword that starts being read will say so by
    /// failing to load.
    fn preflight_probes() -> Vec<(&'static str, String)> {
        use crate::keywords::{
            BLOCK_SECTIONS, LOOP_CONTROL_KEYWORDS, PLAY_KEYWORDS, Support, TASK_KEYWORDS,
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
        // A block's sections are grammar the loader has to accept and the pre-flight has to
        // refuse, even though nothing compiles them yet.
        probes.extend(BLOCK_SECTIONS.iter().map(|s| (*s, task(s))));
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
            90,
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
