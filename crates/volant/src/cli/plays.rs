// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a run's plays come from: the playbook files of `volant playbook`, or the one play of
//! `volant adhoc`.

use std::path::PathBuf;

use super::adhoc::Adhoc;
use super::args::PlaybookArgs;
use crate::playbook::{self, Playbook, Rendering};

pub(super) enum Plays<'a> {
    /// The playbook files `args.playbooks` names.
    Files,
    /// The one play `volant adhoc` builds, shown through the `minimal` callback.
    Adhoc(&'a Adhoc),
    /// The playbook files, run by `volant shadow replay` with every host answered from the
    /// recording.
    Replay(&'a std::sync::Arc<crate::shadow::replay::Replay>),
}

impl Plays<'_> {
    /// The recording a replay answers the hosts from; a replay starts no agent at all.
    pub(super) fn replay(&self) -> Option<std::sync::Arc<crate::shadow::replay::Replay>> {
        match self {
            Plays::Replay(replay) => Some(std::sync::Arc::clone(replay)),
            Plays::Files | Plays::Adhoc(_) => None,
        }
    }

    /// The file each playbook was named by; an ad hoc play is written nowhere, and its directory
    /// is `--playbook-dir` or the working one.
    pub(super) fn files(&self, args: &PlaybookArgs) -> Vec<PathBuf> {
        match self {
            Plays::Files | Plays::Replay(_) => args.playbooks.clone(),
            Plays::Adhoc(adhoc) => vec![adhoc.file()],
        }
    }

    /// The playbooks of `files`, which [`Plays::files`] returned.
    pub(super) fn load(
        &self,
        files: &[PathBuf],
        rendering: &Rendering<'_>,
    ) -> anyhow::Result<Vec<Playbook>> {
        match self {
            Plays::Files | Plays::Replay(_) => files
                .iter()
                .map(|p| playbook::load_rendering(p, rendering))
                .collect(),
            Plays::Adhoc(adhoc) => Ok(vec![adhoc.playbook()?]),
        }
    }
}

/// Runs the playbooks and returns the process exit code.
pub fn run(args: PlaybookArgs) -> i32 {
    if args.version {
        println!("{}", super::announce::version_line());
        return 0;
    }
    super::run::drive(&args, &Plays::Files)
}

/// Runs the playbooks of `args` with every task that would reach a host answered by `replay`,
/// and returns the process exit code.
pub(crate) fn replay(
    args: &PlaybookArgs,
    replay: &std::sync::Arc<crate::shadow::replay::Replay>,
) -> i32 {
    super::run::drive(args, &Plays::Replay(replay))
}
