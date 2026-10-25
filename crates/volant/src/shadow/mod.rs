// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant shadow`: replaying a run `ansible-playbook` recorded with the `volant.compat.record`
//! callback, to show what Volant would have done differently.

mod args;
mod compare;
mod recording;
pub(crate) mod replay;
mod report;
mod watch;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Subcommand;
use sha2::{Digest as _, Sha256};

use crate::runtime::fetch::sha256_of;

/// The exit code of a recording that cannot be replayed against this checkout.
const REFUSED: i32 = 4;

#[derive(Subcommand, Debug)]
pub enum ShadowCommand {
    /// Replay a recorded run through Volant without contacting any host.
    Replay {
        /// The recording, `.jsonl` or `.jsonl.gz`.
        recording: PathBuf,
        /// Report as JSON Lines: one object per divergence, then a summary object.
        #[arg(long)]
        json: bool,
    },
}

pub fn shadow(command: ShadowCommand) -> i32 {
    match command {
        ShadowCommand::Replay { recording, json } => replay(&recording, json),
    }
}

/// Replays `path` and reports what differs: exit 0 for nothing, 1 for at least one divergence.
/// The replayed run prints on stderr, so the report alone is on stdout.
fn replay(path: &Path, json: bool) -> i32 {
    let ready = recording::read(path).and_then(|recording| {
        check(&recording.header, Path::new("."))?;
        let args = args::playbook_args(&recording.header)?;
        Ok((recording, args))
    });
    let (recording, args) = match ready {
        Ok(ready) => ready,
        Err(err) => {
            eprintln!("ERROR! {err}");
            return REFUSED;
        }
    };
    let header = &recording.header;
    if !in_profile(&header.ansible_core) {
        eprintln!(
            "[WARNING]: the recording is of ansible-core {}, outside the 2.19 series Volant              follows: the replay is out-of-profile",
            header.ansible_core
        );
    }
    if recording.truncated {
        eprintln!(
            "[WARNING]: the recording is truncated: the run was interrupted, and the replay              stops where the recording does"
        );
    }
    let root = std::env::current_dir().unwrap_or_default();
    let replay = Arc::new(replay::Replay::new(&recording, &root));
    crate::cli::replay(&args, &replay);
    let report = compare::compare(&recording, &replay);
    print!("{}", if json { report.json() } else { report.human() });
    i32::from(!report.divergences.is_empty())
}

/// Whether a recording of ansible-core `version` is one Volant follows.
fn in_profile(version: &str) -> bool {
    version.starts_with("2.19.")
}

/// Refuses a recording whose playbooks, inventory files or extra-vars files differ in `dir`
/// from what was recorded, naming the first one that does.
fn check(header: &recording::Header, dir: &Path) -> Result<(), String> {
    let files = header.playbooks.iter().chain(&header.inventory.files);
    for file in files {
        let name = relative(&file.path, &header.cwd);
        match sha256_of(&dir.join(name)) {
            Ok(hash) if hash == file.sha256 => {}
            Ok(_) => return Err(format!("{} changed since the recording", name.display())),
            Err(err) => return Err(format!("{} cannot be read: {err}", name.display())),
        }
    }
    let arguments: Vec<&str> = header
        .cli
        .get("extra_vars")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if extra_vars_hash(&arguments, dir) != header.extra_vars_hash {
        return Err("a file the extra vars name (-e @file) changed since the recording".to_owned());
    }
    Ok(())
}

/// `path` as seen from the recording's working directory `cwd`, or as it is when outside it.
fn relative<'a>(path: &'a str, cwd: &str) -> &'a Path {
    Path::new(path).strip_prefix(cwd).unwrap_or(Path::new(path))
}

/// The callback's `extra_vars_hash`: each `-e` argument as given, then the file an `@`
/// argument names, each followed by NUL.
fn extra_vars_hash(arguments: &[&str], dir: &Path) -> String {
    let mut digest = Sha256::new();
    for argument in arguments {
        digest.update(argument.as_bytes());
        digest.update([0]);
        if let Some(file) = argument.strip_prefix('@')
            && let Ok(bytes) = std::fs::read(dir.join(file))
        {
            digest.update(&bytes);
            digest.update([0]);
        }
    }
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests;
