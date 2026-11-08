// SPDX-License-Identifier: GPL-3.0-or-later
//! The stages a playbook goes through before the first connection, each one returning its
//! refusal instead of printing it: `volant playbook` runs them in order and stops at the first
//! error, `volant check` runs them to give a verdict.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::compile::{self, Compiled, TagSelection};
use crate::config::Config;
use crate::playbook::{self, Play, Playbook};
use crate::roles::RoleSearch;
use crate::stats::Refusal;
use crate::{preflight, python};

/// Every play of every playbook, compiled: roles read and spliced, imports read. A role nobody
/// can find keeps the code its refusal measured; anything else the compiler refuses exits 4, the
/// code the reference gives a playbook it cannot make sense of.
pub(super) fn compile_all(
    playbooks: &[Playbook],
    config: &Config,
    selection: &TagSelection,
    extra: &Map<String, Value>,
) -> anyhow::Result<Vec<Vec<Compiled>>> {
    let mut compiled = Vec::new();
    for pb in playbooks {
        let mut plays = Vec::new();
        for play in &pb.plays {
            let search = RoleSearch::new(&play.dir, config);
            let play = compile::compile_with(play, &search, selection, extra);
            plays.push(play.map_err(|e| Refusal::or(4, e))?);
        }
        compiled.push(plays);
    }
    Ok(compiled)
}

/// The second pass of the pre-flight, over what the compilation made of each play: a role's tasks
/// and the files an import spliced in. `prompts` says, per play, whether a prompting `pause`
/// could block it.
pub(super) fn check_compiled(
    playbooks: &[Playbook],
    compiled: &[Vec<Compiled>],
    prompts: impl Fn(&Play) -> bool,
) -> anyhow::Result<()> {
    for (pb, steps) in playbooks.iter().zip(compiled) {
        for (play, compiled) in pb.plays.iter().zip(steps) {
            preflight::check_steps(compiled, prompts(play))?;
        }
    }
    Ok(())
}

/// Every Python module the compiled plays name or a dynamic include may read, and the plugin
/// directories they bring, with the run's own action plugins noted. `files` are the playbooks as
/// named, whose directories are searched first.
pub(super) fn reach_of(
    files: &[PathBuf],
    playbooks: &[Playbook],
    compiled: &[Vec<Compiled>],
) -> anyhow::Result<python::Reach> {
    let named_dirs: Vec<PathBuf> = files
        .iter()
        .map(PathBuf::as_path)
        .map(playbook::base_dir)
        .collect();
    let named_playbooks: Vec<python::Named> = named_dirs
        .iter()
        .zip(playbooks.iter().zip(compiled))
        .map(|(dir, (pb, steps))| python::Named {
            dir: Some(dir.as_path()),
            plays: pb
                .plays
                .iter()
                .map(|play| play.dir.as_path())
                .zip(steps)
                .collect(),
        })
        .collect();
    let reach = python::Reach::walk_run(&named_playbooks).map_err(|e| Refusal::or(4, e))?;
    reach.note_own_actions();
    python::check_plugin_order(&reach).map_err(|e| Refusal::or(4, e))?;
    Ok(reach)
}

/// Every `(task, module)` of the compiled plays whose module only the controller's ansible-core
/// can resolve.
pub(super) fn collection_names(compiled: &[Vec<Compiled>]) -> Vec<(String, String)> {
    compiled
        .iter()
        .flatten()
        .flat_map(preflight::collection_modules)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(dir: &std::path::Path, yaml: &str) -> Vec<Playbook> {
        let path = dir.join("site.yml");
        std::fs::write(&path, yaml).unwrap();
        vec![playbook::load(&path).unwrap()]
    }

    fn refusal(err: &anyhow::Error) -> (i32, String) {
        let refusal = err.downcast_ref::<Refusal>().expect("a refusal");
        (refusal.code, refusal.message.clone())
    }

    #[test]
    fn a_missing_role_is_refused_with_the_runs_code_and_message() {
        let dir = tempfile::tempdir().unwrap();
        let playbooks = loaded(dir.path(), "- hosts: all\n  roles: [nosuchrole]\n");
        let err = compile_all(
            &playbooks,
            &Config::default(),
            &TagSelection::default(),
            &Map::new(),
        )
        .unwrap_err();
        let (code, message) = refusal(&err);
        assert_eq!(code, 1);
        assert!(
            message.contains("the role 'nosuchrole' was not found"),
            "{message}"
        );
    }

    #[test]
    fn a_pause_that_would_block_is_refused_with_code_4() {
        let dir = tempfile::tempdir().unwrap();
        let playbooks = loaded(
            dir.path(),
            "- hosts: all\n  tasks:\n    - pause: {prompt: Go?, seconds: 1}\n",
        );
        let compiled = compile_all(
            &playbooks,
            &Config::default(),
            &TagSelection::default(),
            &Map::new(),
        )
        .unwrap();
        check_compiled(&playbooks, &compiled, |_| false).unwrap();
        let err = check_compiled(&playbooks, &compiled, |_| true).unwrap_err();
        let (code, message) = refusal(&err);
        assert_eq!(code, 4);
        assert!(message.contains(preflight::PROMPT_REFUSED), "{message}");
    }
}
