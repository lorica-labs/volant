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
}

impl Plays<'_> {
    /// The file each playbook was named by; an ad hoc play is written nowhere, and its directory
    /// is `--playbook-dir` or the working one.
    pub(super) fn files(&self, args: &PlaybookArgs) -> Vec<PathBuf> {
        match self {
            Plays::Files => args.playbooks.clone(),
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
            Plays::Files => files
                .iter()
                .map(|p| playbook::load_rendering(p, rendering))
                .collect(),
            Plays::Adhoc(adhoc) => Ok(vec![adhoc.playbook()?]),
        }
    }
}
