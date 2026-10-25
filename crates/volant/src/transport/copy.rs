// SPDX-License-Identifier: GPL-3.0-or-later
//! The `copy` upload, for an exec target with no shell (distroless, scratch): the runtime's own
//! copy command puts the agent in the target's `/tmp`, the agent places itself in its cache
//! directory under the rules the upload script applies (`--place`), and the controller starts it
//! there. Nothing in the target runs but the agent.
//!
//! The architecture comes from the runtime's metadata, since nothing in the target can be asked.
//! The agent starts without the shell's last check of its directory: it placed itself there and
//! checked it the moment before, and a target with no shell has one account in practice.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::agent::{AgentLink, AgentSource};

use super::bootstrap::{Captured, first_words, is_this_agent, triple_for};
use super::defaults::ConnectError;
use super::exec::ExecTarget;
use super::reach::Reach;

/// The directory the copy lands in before the agent places itself.
const LANDING: &str = "/tmp";

/// How an exec target's runtime copies a file in and says what machine the target is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CopyInto {
    /// The command that reads a tar stream on stdin and unpacks it into a directory of the target,
    /// with `{dir}` in the word where the directory goes: `docker cp - app1:{dir}`.
    pub tar_into: Vec<String>,
    /// The commands whose last answer names the target's architecture, each with `{}` in a word
    /// for the previous one's answer: the container's image, then that image's architecture.
    pub arch: Vec<Vec<String>>,
}

/// Where each target placed its agent this run, so a second link starts it without a copy.
static PLACED: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

impl ExecTarget {
    /// Whether the first probe failed because the target has no `/bin/sh`. Docker 29 prints the
    /// runtime's refusal on stdout.
    pub(super) fn has_no_shell(probe: &Captured) -> bool {
        matches!(probe.code, Some(126 | 127))
            && (probe.stderr.contains("/bin/sh") || probe.stdout.contains("/bin/sh"))
    }

    /// The link to a target with no shell, through the agent placing itself.
    pub(super) async fn connect_copied(
        &self,
        copy: &CopyInto,
        agents: &AgentSource,
    ) -> Result<AgentLink, ConnectError> {
        let key = format!("{}\0{}", self.prefix.join("\0"), self.remote_tmp);
        let arch = self.architecture(copy).await?;
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
        let file = {
            let (agents, path) = (agents.clone(), local.clone());
            tokio::task::spawn_blocking(move || agents.read(&path))
                .await
                .unwrap_or_else(|e| Err(std::io::Error::other(e)))
                .map_err(|e| {
                    ConnectError::Unreachable(format!("reading {}: {e}", local.display()))
                })?
        };
        let known = PLACED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|placed| placed.get(&key).cloned());
        let program = match known {
            Some(program) if self.answers(&program, &file.hash).await? => program,
            _ => self.place(copy, &file.bytes, &file.hash).await?,
        };
        PLACED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .insert(key, program.clone());
        let argv = self.program(&program, &[]);
        self.trace_exec(&argv);
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ConnectError::Unreachable(format!("starting {}: {e}", argv[0])))?;
        AgentLink::new_with_preamble(child, None)
            .await
            .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))
    }

    /// The architecture the runtime's metadata names, as `uname -m` would.
    async fn architecture(&self, copy: &CopyInto) -> Result<String, ConnectError> {
        let mut answer = String::new();
        for step in &copy.arch {
            let argv: Vec<String> = step
                .iter()
                .map(|word| word.replace("{}", &answer))
                .collect();
            let said = self.capture(&argv, None).await?;
            if said.code != Some(0) {
                return Err(ConnectError::Unreachable(format!(
                    "{} has no shell, and asking {} what machine it is failed: {}",
                    self.name,
                    argv[0],
                    first_words(&said.stderr, "it exited without a message")
                )));
            }
            answer = said.stdout.trim().to_string();
        }
        Ok(match answer.as_str() {
            "amd64" => "x86_64".to_string(),
            "arm64" => "aarch64".to_string(),
            other => other.to_string(),
        })
    }

    /// Copies the agent in, has it place itself, and checks that the placed copy is this one.
    async fn place(
        &self,
        copy: &CopyInto,
        bytes: &[u8],
        hash: &str,
    ) -> Result<String, ConnectError> {
        let landed = format!("volant-agent-{}-{}", env!("CARGO_PKG_VERSION"), &hash[..16]);
        let tar = super::tar::single_file(&landed, 0o755, bytes);
        let argv: Vec<String> = copy
            .tar_into
            .iter()
            .map(|word| word.replace("{dir}", LANDING))
            .collect();
        let copied = self.capture(&argv, Some(&tar)).await?;
        if copied.code != Some(0) {
            return Err(ConnectError::Unreachable(format!(
                "copying the agent into {}:{LANDING} failed: {}",
                self.name,
                first_words(&copied.stderr, "the copy exited without a message")
            )));
        }
        let landed = format!("{LANDING}/{landed}");
        let placed = self
            .capture(
                &self.program(&landed, &["--place", &self.remote_tmp, "--from-copy"]),
                None,
            )
            .await?;
        let program = match placed.code {
            Some(0) => placed.stdout.trim().to_string(),
            Some(47) => {
                return Err(ConnectError::Unreachable(format!(
                    "the agent directory on {} is not safe to run from, so nothing was run: {}",
                    self.name,
                    first_words(&placed.stderr, "the agent did not say why")
                )));
            }
            _ => {
                return Err(ConnectError::Unreachable(format!(
                    "uploading the agent failed: {}",
                    first_words(&placed.stderr, "the agent said nothing")
                )));
            }
        };
        if !self.answers(&program, hash).await? {
            return Err(ConnectError::Unreachable(format!(
                "the uploaded agent at {program} is not the binary this controller sent"
            )));
        }
        Ok(program)
    }

    /// Whether the agent at `program` is this controller's build.
    async fn answers(&self, program: &str, hash: &str) -> Result<bool, ConnectError> {
        let said = self
            .capture(&self.program(program, &["--version", "--build-id"]), None)
            .await?;
        // `is_this_agent` reads the second line, after the architecture a shell probe prints.
        let probe = Captured {
            code: said.code,
            stdout: format!("-\n{}", said.stdout),
            stderr: said.stderr,
        };
        Ok(is_this_agent(&probe, hash))
    }

    /// The command line that runs `program` with `args` in the target, with no shell.
    fn program(&self, program: &str, args: &[&str]) -> Vec<String> {
        let mut argv = self.prefix.clone();
        argv.push(program.to_string());
        argv.extend(args.iter().map(ToString::to_string));
        argv
    }

    /// Runs one local command to completion, bounded by the connection timeout.
    async fn capture(
        &self,
        argv: &[String],
        stdin: Option<&[u8]>,
    ) -> Result<Captured, ConnectError> {
        self.trace_exec(argv);
        let run = async {
            let mut child = Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(if stdin.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
                let _ = pipe.write_all(bytes).await;
            }
            child.wait_with_output().await
        };
        // A copy of several megabytes is given more than the bare connection timeout.
        let output = tokio::time::timeout(self.connect_timeout * 6, run)
            .await
            .map_err(|_| {
                ConnectError::Unreachable(format!(
                    "{} did not finish within {} seconds",
                    argv[0],
                    (self.connect_timeout * 6).as_secs()
                ))
            })?
            .map_err(|e| ConnectError::Unreachable(format!("running {}: {e}", argv[0])))?;
        let captured = Captured {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        self.trace_result(&captured);
        Ok(captured)
    }
}
