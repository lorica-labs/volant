// SPDX-License-Identifier: GPL-3.0-or-later
//! Putting the agent on a host: the probe, the upload and the commands they run.

use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::agent::AgentSource;

use super::defaults::{ConnectError, SshTarget};
use super::escalation::unreachable_message;
use super::escalation::{RunAs, escalated_command, preamble};

/// Exit codes the bootstrap shell uses to talk back before any frame exists. They are picked
/// above every code the shell itself hands out (126, 127) and below 255, which belongs to
/// `ssh`, so a code seen by `run_capturing` names exactly one situation.
///
/// Every command the bootstrap runs replaces the status of the programs it calls with one of
/// these, so none of them can hand back 255 and be read as `ssh`'s own failure: the cached
/// agent's status becomes `EXIT_AGENT_UNRUNNABLE` and `uname`'s becomes `EXIT_UNAME_FAILED`.
pub(super) const EXIT_AGENT_MISSING: i32 = 42;
pub(super) const EXIT_NO_SPACE: i32 = 43;
pub(super) const EXIT_SHORT_WRITE: i32 = 44;
pub(super) const EXIT_AGENT_UNRUNNABLE: i32 = 45;
pub(super) const EXIT_UNAME_FAILED: i32 = 46;
/// `ssh` reports its own failures with this, whatever the remote command would have returned.
pub(super) const EXIT_SSH_FAILURE: i32 = 255;

/// Whether the bootstrap probe itself answered, which is what an escalation probe has to
/// establish before anything is written to the link. Every exit the probe script can make is
/// one of these, and nothing else on the path produces one: `sudo` refuses with its own 1, and
/// a shell that cannot find or run it says 126 or 127. So one of these codes proves the command
/// really did run as the asked-for user, whatever it then found there.
pub(super) fn bootstrap_probe_ran(probe: &Captured) -> bool {
    matches!(
        probe.code,
        Some(0 | EXIT_AGENT_MISSING | EXIT_AGENT_UNRUNNABLE | EXIT_UNAME_FAILED)
    )
}

impl SshTarget {
    /// One bootstrap command, run as the escalated user where the link escalates.
    ///
    /// The `sudo` password goes on stdin ahead of any payload, never after it: with the
    /// `ReadStdin` form `sudo` reads exactly one line before the command it starts sees
    /// anything, so an upload whose first line went to `sudo` would land one line short.
    pub(super) async fn run_bootstrap(
        &self,
        run_as: RunAs<'_>,
        command: &str,
        payload: Option<&[u8]>,
    ) -> Result<Captured, ConnectError> {
        let escalated = match run_as {
            RunAs::Direct(escalated) => escalated,
            RunAs::Su(escalation) => return self.run_su(escalation, command, payload).await,
        };
        let secret = preamble(escalated);
        // A password is only ever on this pipe when `secret` carries it; asking the escalation
        // directly would say "offered" for a `NoPrompt` probe that never wrote it anywhere.
        let offered = secret
            .is_some()
            .then(|| escalated.and_then(|(e, _)| e.password.as_deref()))
            .flatten();
        let stdin = match (secret, payload) {
            (None, payload) => payload.map(<[u8]>::to_vec),
            (Some(secret), None) => Some(secret),
            (Some(secret), Some(payload)) => Some([secret.as_slice(), payload].concat()),
        };
        let command = match escalated {
            None => command.to_string(),
            // The messages `escalation_outcome` matches are `sudo`'s English ones.
            Some(_) => format!("LC_ALL=C {}", escalated_command(escalated, command)),
        };
        self.run_capturing(&command, stdin.as_deref(), offered)
            .await
    }

    /// Makes sure the host has this exact agent binary cached for the link's target user,
    /// uploading it if it does not. Every way out other than `Ok(())` is a `ConnectError`, so
    /// no caller can go on to run a missing, stale, foreign or wrong-architecture agent.
    /// `probed` is that first probe already run, which is what the escalation check runs to
    /// settle the `sudo` form.
    #[expect(clippy::too_many_lines)]
    pub(super) async fn bootstrap(
        &self,
        agents: &AgentSource,
        run_as: RunAs<'_>,
        probed: Option<Captured>,
    ) -> Result<(), ConnectError> {
        let probe = match probed {
            Some(probe) => probe,
            None => {
                self.run_bootstrap(run_as, &self.probe_command(), None)
                    .await?
            }
        };
        // Exits 0, 42 and 45 all come after the probe printed the architecture on its first
        // line. A cached agent that cannot run (45) takes the same path as a cached build other
        // than this controller's: this controller wrote that file with a verified byte count
        // and an atomic rename, but a truncated or `ENOEXEC` file it left behind before that
        // check existed, or after a hard-killed run, wears the same symptom, and it used to heal
        // on the next run. So exit 45 gets one more upload before it is reported; the
        // post-upload re-probe is what errors if a fresh copy still cannot run. Anything else
        // is a remote shell that failed outright, so its stdout cannot be read as an
        // architecture. Ask the machine again, separately.
        let arch = match probe.code {
            Some(0 | EXIT_AGENT_MISSING | EXIT_AGENT_UNRUNNABLE) => {
                probe.stdout.lines().next().unwrap_or("").trim().to_string()
            }
            Some(EXIT_UNAME_FAILED) => {
                return Err(ConnectError::Unreachable(format!(
                    "the remote shell failed: {}",
                    first_words(&probe.stderr, "'uname -m' said nothing")
                )));
            }
            _ => {
                let uname = self
                    .run_bootstrap(
                        run_as,
                        &format!("uname -m || exit {EXIT_UNAME_FAILED}"),
                        None,
                    )
                    .await?;
                if uname.code != Some(0) {
                    return Err(ConnectError::Unreachable(format!(
                        "the remote shell failed: {}",
                        first_words(&uname.stderr, "uname -m returned no output")
                    )));
                }
                uname.stdout.trim().to_string()
            }
        };
        if arch.is_empty() {
            return Err(ConnectError::Unreachable(
                "the host did not say what architecture it is: 'uname -m' printed nothing"
                    .to_string(),
            ));
        }
        let triple = triple_for(&arch).ok_or_else(|| {
            ConnectError::Unreachable(format!(
                "no agent binary for {arch}: unsupported architecture"
            ))
        })?;
        let local = agents.for_target(triple).map_err(|looked| {
            ConnectError::Unreachable(format!(
                "no agent binary for {arch} (looked for volant-agent-{triple} in {looked})"
            ))
        })?;
        // Read and hashed once per run, off the async workers: the file is several megabytes.
        let file = {
            let (agents, path) = (agents.clone(), local.clone());
            tokio::task::spawn_blocking(move || agents.read(&path))
                .await
                .unwrap_or_else(|e| Err(std::io::Error::other(e)))
                .map_err(|e| {
                    ConnectError::Unreachable(format!("reading {}: {e}", local.display()))
                })?
        };
        if is_this_agent(&probe, &file.hash) {
            return Ok(());
        }
        let size = file.bytes.len() as u64;
        let upload = self
            .run_bootstrap(run_as, &self.upload_command(size), Some(&file.bytes))
            .await?;
        match upload.code {
            Some(0) => {}
            Some(EXIT_NO_SPACE) => {
                return Err(ConnectError::Unreachable(format!(
                    "not enough space in {}: need {} KiB, {} available",
                    self.remote_tmp,
                    space_needed_kib(size),
                    first_words(&upload.stdout, "an unknown amount")
                )));
            }
            Some(EXIT_SHORT_WRITE) => {
                return Err(ConnectError::Unreachable(
                    "uploading the agent failed: the transfer was cut short".to_string(),
                ));
            }
            _ => {
                return Err(ConnectError::Unreachable(format!(
                    "uploading the agent failed: {}",
                    first_words(&upload.stderr, "the remote shell said nothing")
                )));
            }
        }
        // Ask the host what it now has rather than trusting the write. A persisting 45 names
        // the real cause instead of the generic mismatch message: the freshly uploaded copy is
        // there, and still cannot run, so the fault is the host's (a `noexec` mount, SELinux,
        // a wrapper script or a foreign architecture), not a stale file this controller can fix
        // by trying again.
        let check = self
            .run_bootstrap(run_as, &self.probe_command(), None)
            .await?;
        if check.code == Some(EXIT_AGENT_UNRUNNABLE) {
            return Err(ConnectError::Unreachable(format!(
                "the cached agent {} cannot be run: {}",
                self.agent_path(),
                first_words(&check.stderr, "it failed without a message")
            )));
        }
        if !is_this_agent(&check, &file.hash) {
            return Err(ConnectError::Unreachable(format!(
                "the uploaded agent is not the binary this controller sent: the host's copy answered '{}'",
                check.stdout.lines().nth(1).unwrap_or("").trim()
            )));
        }
        Ok(())
    }

    /// Runs one remote command to completion, feeding `stdin` if given, and returns its
    /// output. An `ssh` exit of 255 is a connection failure, reported with ssh's own words.
    ///
    /// `offered` is the `sudo` password when `stdin` is the `ReadStdin` preamble carrying it,
    /// and `None` otherwise (including when `stdin` is an upload's bytes, which are never a
    /// password). It exists so the 255 message can be scrubbed the same way `escalation_outcome`
    /// scrubs `sudo`'s own refusal: this is the only other place that password is ever written
    /// to a child's stdin.
    async fn run_capturing(
        &self,
        remote_command: &str,
        stdin: Option<&[u8]>,
        offered: Option<&str>,
    ) -> Result<Captured, ConnectError> {
        let argv = self.ssh_argv_with(remote_command, stdin.is_some());
        super::trace::exec(self, &argv);
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .envs(crate::askpass::environment())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let mut child = command
            .spawn()
            .map_err(|e| ConnectError::Unreachable(format!("starting ssh: {e}")))?;
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A remote side that gives up early closes the pipe; the exit code below says
            // what happened, so a broken pipe here is not the error worth reporting.
            let _ = pipe.write_all(bytes).await;
            drop(pipe);
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| ConnectError::Unreachable(format!("waiting for ssh: {e}")))?;
        let captured = Captured {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        super::trace::result(self, &captured);
        // `ssh` reports its own failures as 255 and none of the bootstrap commands ever
        // returns it, so this cannot swallow a remote command's own status.
        if captured.code == Some(EXIT_SSH_FAILURE) {
            return Err(ConnectError::Unreachable(unreachable_message(
                &captured.stderr,
                offered,
            )));
        }
        Ok(captured)
    }
}

#[derive(Debug)]
pub(super) struct Captured {
    pub(super) code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

/// What a stream said, or `fallback` when it said nothing worth printing.
pub(super) fn first_words(text: &str, fallback: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Whether the probe ran exactly the binary hashed as `hash` from the cache: the agent it found
/// printed this version and a hash of its own file equal to the blake3 hash of the binary this
/// controller would upload. The version alone proves nothing, since any build of this source
/// reports it.
pub(super) fn is_this_agent(probe: &Captured, hash: &str) -> bool {
    let expected = format!("volant-agent {} {hash}", env!("CARGO_PKG_VERSION"));
    probe.code == Some(0) && probe.stdout.lines().nth(1).map(str::trim) == Some(expected.as_str())
}

/// `uname -m` to the agent build we ship for it.
pub fn triple_for(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" | "amd64" => Some("x86_64-unknown-linux-musl"),
        "aarch64" | "arm64" => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

/// How much room the cache directory needs, in KiB, before an upload starts: the incoming
/// copy, the previously cached version that is only removed once the new one has landed, and
/// a mebibyte on top so a nearly full filesystem is refused rather than filled.
pub fn space_needed_kib(size: u64) -> u64 {
    size * 2 / 1024 + 1024
}
