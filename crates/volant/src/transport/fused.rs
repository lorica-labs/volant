// SPDX-License-Identifier: GPL-3.0-or-later
//! A link opened in one `ssh` rather than two: the bootstrap probe and the start of the agent in
//! a single command, tried first and given up without a word when the host is not ready for it.
//!
//! The probe's job is to say whether the cached agent is this controller's build, and, for a link
//! that escalates, which `sudo` form the host takes. Both are known in advance on a host that ran
//! before: the controller holds the build each machine type gets, and `sudo -n` is the form the
//! probe tries first. So the command checks the directory as the probe does, compares the cached
//! agent's `--version --build-id` with the build for this `uname -m`, and starts it under
//! `sudo -n`. Anything else, a missing or other build, a directory the checks refuse, a `sudo` that
//! wants a password, ends the attempt before the handshake, and the probe, the upload and the
//! start run as they always do, with the words they always use.

use std::collections::BTreeSet;
use std::process::Stdio;
use std::sync::Mutex;

use std::io::Write as _;

use tokio::io::AsyncReadExt;
use tokio::process::{ChildStderr, Command};
use tokio::sync::oneshot;

use crate::agent::{AgentLink, AgentSource};

use super::bootstrap::{EXIT_NOT_FUSED, triple_for};
use super::defaults::{BecomeMethod, Escalation, SshTarget};
use super::escalation::{SudoForm, escalated_command};

/// The machine types the controller ships an agent for, as `case` patterns of `uname -m`, with
/// the name [`triple_for`] knows each by.
const MACHINES: [(&str, &str); 2] = [("x86_64|amd64", "x86_64"), ("aarch64|arm64", "aarch64")];

/// The hosts, users and escalations of this run whose attempt failed for a reason a later link
/// would meet again (a `sudo` that wants a password): they open the long way from then on.
static REFUSED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

impl SshTarget {
    /// The link, opened in one `ssh`, or `None` when it has to be opened the long way.
    pub(super) async fn open_fused(
        &self,
        agents: &AgentSource,
        escalation: Option<&Escalation>,
    ) -> Option<AgentLink> {
        if escalation.is_some_and(|e| e.method != BecomeMethod::Sudo) {
            return None;
        }
        let key = format!(
            "{}:{}:{}:{}",
            self.address,
            self.port.unwrap_or(22),
            self.user.as_deref().unwrap_or(""),
            escalation.map_or("", |e| e.user.as_str())
        );
        if lock().contains(&key) {
            return None;
        }
        let expected = expected(agents).await;
        if expected.is_empty() {
            return None;
        }
        let fused = self.fused_command(&expected);
        let escalated = escalation.map(|e| (e, SudoForm::NoPrompt));
        let remote = match escalated {
            None => fused,
            Some(_) => format!("exec {}", escalated_command(escalated, &fused)),
        };
        let argv = self.ssh_argv(&remote);
        super::trace::exec(self, &argv);
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .envs(self.askpass_environment().ok()?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let held = Held::new(child.stderr.take()?);
        let mut link = AgentLink::new(child).ok()?;
        if let Ok(Ok(())) = tokio::time::timeout(self.connect_timeout, link.handshake()).await {
            held.release(true);
            return Some(link);
        }
        held.release(false);
        // A build that is missing or another one is mended by the upload; anything else would
        // refuse the next link the same way.
        if link.exit_code().await != Some(EXIT_NOT_FUSED) {
            lock().insert(key);
        }
        None
    }
}

fn lock() -> std::sync::MutexGuard<'static, BTreeSet<String>> {
    REFUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The line each machine type's agent answers `--version --build-id` with, for the agents this
/// controller holds. Each file is read and hashed once per run.
async fn expected(agents: &AgentSource) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (pattern, machine) in MACHINES {
        let Some(path) = triple_for(machine).and_then(|t| agents.for_target(t).ok()) else {
            continue;
        };
        let agents = agents.clone();
        let read = tokio::task::spawn_blocking(move || agents.read(&path)).await;
        if let Ok(Ok(file)) = read {
            let line = format!("volant-agent {} {}", env!("CARGO_PKG_VERSION"), file.hash);
            out.push((pattern, line));
        }
    }
    out
}

/// The link's stderr, held back until the handshake says whose it is: the agent's, copied on to
/// the controller's stderr from then on, or a refusing `sudo`'s, dropped unread.
struct Held(oneshot::Sender<bool>);

impl Held {
    fn new(mut pipe: ChildStderr) -> Held {
        let (tx, mut rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut held = Vec::new();
            let mut chunk = [0u8; 4096];
            let forward = loop {
                tokio::select! {
                    decided = &mut rx => break decided.unwrap_or(false),
                    read = pipe.read(&mut chunk) => match read {
                        Ok(0) | Err(_) => break (&mut rx).await.unwrap_or(false),
                        Ok(n) => held.extend_from_slice(&chunk[..n]),
                    },
                }
            };
            if !forward {
                return;
            }
            let _ = std::io::stderr().write_all(&held);
            while let Ok(n @ 1..) = pipe.read(&mut chunk).await {
                let _ = std::io::stderr().write_all(&chunk[..n]);
            }
        });
        Held(tx)
    }

    fn release(self, forward: bool) {
        let _ = self.0.send(forward);
    }
}
