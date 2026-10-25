// SPDX-License-Identifier: GPL-3.0-or-later
//! A target reached through a local command that runs another command inside it with stdin and
//! stdout passed through: `docker exec -i`, `kubectl exec -i`, `lxc exec`, `chroot`. The agent's
//! link already is one process whose stdin and stdout carry frames, so any such command carries
//! it, and the probe, the upload and the start are the ones `ssh` runs.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::agent::{AgentLink, AgentSource};

use super::agent_dir::AgentCommands;
use super::bootstrap::{Captured, bootstrap, bootstrap_probe_ran, first_words, run_bootstrap};
use super::defaults::{BecomeMethod, ConnectError, Escalation};
use super::escalation::{RunAs, escalated_command, preamble, preparing_for, settle_form};
use super::reach::Reach;

/// How the `-vvvv` lines show a command, as the reference's plugin does: docker writes
/// `ESTABLISH` then `EXEC [...]` in text, podman writes `RUN [...]` in bytes and no `ESTABLISH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lines {
    Exec,
    Run,
    /// `THIS IS A LOCAL CHROOT DIR` for the link, then `EXEC [...]` lines.
    Chroot,
    /// `ESTABLISH <KIND> CONNECTION` with no user, then `EXEC [...]` lines: kubectl's.
    NoUser,
}

/// A file the controller wrote for a target's commands to read, such as a kubeconfig given as a
/// mapping; removed when the last copy of the target that names it goes.
#[derive(Debug, Clone)]
pub struct KeptFile(pub std::sync::Arc<tempfile::TempPath>);

impl PartialEq for KeptFile {
    fn eq(&self, other: &KeptFile) -> bool {
        **self.0 == **other.0
    }
}

impl Eq for KeptFile {}

impl std::hash::Hash for KeptFile {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self.0).hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecTarget {
    /// The container, pod or directory, shown in the `<...>` of the `-vvvv` lines.
    pub name: String,
    /// The connection's word in `ESTABLISH <KIND> CONNECTION`: `DOCKER` for docker.
    pub kind: String,
    /// How the `-vvvv` lines show a command.
    pub lines: Lines,
    /// The shell every command but the agent's own start runs under: `/bin/sh` unless the
    /// connection's `executable` says otherwise.
    pub shell: String,
    /// The command line that runs a program in the target with stdin and stdout passed
    /// through, up to the program: `docker exec -i app1`.
    pub prefix: Vec<String>,
    /// The user the commands run as, when the runtime is told one.
    pub user: Option<String>,
    /// With no user set, the command whose output names the user the target runs as, for the
    /// `ESTABLISH` line: `docker inspect --format {{.Config.User}}`.
    pub user_probe: Option<Vec<String>>,
    /// Why the target cannot be reached at all, said when a link is asked for rather than when
    /// the connection is read: the runtime's program is not installed here.
    pub unavailable: Option<String>,
    /// How the runtime copies the agent in when the target has no shell; `None` for a runtime
    /// whose targets always have one.
    pub copy: Option<super::CopyInto>,
    /// Whether the agent's stderr is dropped rather than shown: rootless buildah writes a dozen
    /// warnings about capabilities there for every command it runs.
    pub quiet_stderr: bool,
    /// The plugin's own words for a runtime refusal of the first command: when the runtime's
    /// output holds the first text, the target is unreachable with the second, rather than with
    /// what the runtime said.
    pub failures: Vec<(String, String)>,
    /// A `-vvvv` line the plugin prints after its `ESTABLISH` line, such as lxd's note on a user
    /// other than root.
    pub info: Option<String>,
    /// Variables set in the environment of every local command, kubectl's `local_env_vars`.
    pub env: Vec<(String, String)>,
    /// Values masked as `********` in every command line printed, such as a token.
    pub secrets: Vec<String>,
    /// A file the commands read, kept for as long as the target is.
    pub kept: Option<KeptFile>,
    pub remote_tmp: String,
    /// How long the runtime may take to answer the first command before the target is
    /// reported unreachable.
    pub connect_timeout: Duration,
}

impl ExecTarget {
    /// The command line that runs `program` with `args` in the target.
    fn run(&self, program: &str, args: &[&str]) -> Vec<String> {
        let mut argv = self.prefix.clone();
        argv.push(program.to_string());
        argv.extend(args.iter().map(ToString::to_string));
        argv
    }

    /// Where the agent's stderr goes.
    pub(super) fn link_stderr(&self) -> Stdio {
        if self.quiet_stderr {
            Stdio::null()
        } else {
            Stdio::inherit()
        }
    }

    /// The connection's name in messages: `docker` for `DOCKER`.
    fn connection(&self) -> String {
        self.kind.to_ascii_lowercase()
    }

    /// Opens a link: the probe, the upload when the target's cache is stale, and the agent's
    /// start, each through the exec command. `sudo` goes ahead of the agent as it does over
    /// `ssh`; `su` reads its password from a terminal, which this command does not give.
    pub(super) async fn connect(
        &self,
        agents: &AgentSource,
        escalation: Option<&Escalation>,
    ) -> Result<AgentLink, ConnectError> {
        if let Some(why) = &self.unavailable {
            return Err(ConnectError::Unreachable(why.clone()));
        }
        if super::trace::connection_lines() && self.lines == Lines::Chroot {
            super::trace::local_chroot(&self.name);
        }
        if super::trace::connection_lines() && self.lines == Lines::NoUser {
            super::trace::note(&self.name, &format!("ESTABLISH {} CONNECTION", self.kind));
        }
        if super::trace::connection_lines() && self.lines == Lines::Exec {
            let user = match (&self.user, &self.user_probe) {
                (Some(user), _) => user.clone(),
                (None, Some(probe)) => self.default_user(probe).await,
                (None, None) => "None".to_string(),
            };
            super::trace::established_as(&self.name, &self.kind, Some(&user));
            if let Some(info) = &self.info {
                super::trace::note(&self.name, info);
            }
        }
        if let Some(e) = escalation
            && e.method == BecomeMethod::Su
        {
            return Err(ConnectError::Become(format!(
                "become_method 'su' is not supported yet on a {} connection",
                self.connection()
            )));
        }
        if escalation.is_some() && self.copy.is_some() {
            let shell = self
                .answering(run_bootstrap(self, RunAs::Direct(None), "exit 0", None))
                .await??;
            if Self::has_no_shell(&shell) {
                return Err(ConnectError::Become(format!(
                    "become needs a shell in {}, which has no /bin/sh",
                    self.name
                )));
            }
        }
        let probe_command = self.probe_command();
        let (escalated, probe) = if let Some(e) = escalation {
            let (form, probe) = self
                .answering(settle_form(e, bootstrap_probe_ran, |form| {
                    run_bootstrap(self, RunAs::Direct(Some((e, form))), &probe_command, None)
                }))
                .await??;
            (Some((e, form)), probe)
        } else {
            let probe = self
                .answering(run_bootstrap(
                    self,
                    RunAs::Direct(None),
                    &probe_command,
                    None,
                ))
                .await??;
            if let Some(copy) = &self.copy
                && Self::has_no_shell(&probe)
            {
                return self.connect_copied(copy, agents).await;
            }
            (None, probe)
        };
        bootstrap(self, agents, RunAs::Direct(escalated), Some(probe))
            .await
            .map_err(|err| preparing_for(err, escalation))?;
        let start = self.exec_command("");
        let start = match escalated {
            None => start,
            Some(_) => format!("exec {}", escalated_command(escalated, &start)),
        };
        let argv = self.command_line(&start, true);
        self.trace_exec(&argv);
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .envs(self.environment()?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(self.link_stderr())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ConnectError::Unreachable(format!("starting {}: {e}", argv[0])))?;
        AgentLink::new_with_preamble(child, preamble(escalated))
            .await
            .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))
    }

    /// The user `probe` names, `root` when it names none, and `?` when it fails, as the
    /// reference's docker plugin prints it.
    async fn default_user(&self, probe: &[String]) -> String {
        let asked = Command::new(&probe[0])
            .args(&probe[1..])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(self.connect_timeout, asked).await {
            Ok(Ok(out)) if out.status.success() => {
                let user = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if user.is_empty() {
                    "root".to_string()
                } else {
                    user
                }
            }
            _ => "?".to_string(),
        }
    }

    /// `step` bounded by the connection timeout: a runtime that hangs makes the target
    /// unreachable, saying how long it waited and for what.
    async fn answering<F: Future>(&self, step: F) -> Result<F::Output, ConnectError> {
        tokio::time::timeout(self.connect_timeout, step)
            .await
            .map_err(|_| {
                let seconds = self.connect_timeout.as_secs();
                ConnectError::Unreachable(format!(
                    "{} did not answer within {seconds} second{} when asked to run a command in {}",
                    self.prefix[0],
                    if seconds == 1 { "" } else { "s" },
                    self.name
                ))
            })
    }
}

impl Reach for ExecTarget {
    fn place(&self) -> &str {
        &self.name
    }

    fn remote_tmp(&self) -> &str {
        &self.remote_tmp
    }

    fn command_line(&self, command: &str, _payload: bool) -> Vec<String> {
        self.run(&self.shell, &["-c", command])
    }

    fn environment(&self) -> Result<Vec<(std::ffi::OsString, std::ffi::OsString)>, ConnectError> {
        Ok(self
            .env
            .iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect())
    }

    fn trace_exec(&self, argv: &[String]) {
        let masked: Vec<String> = argv
            .iter()
            .map(|word| {
                if self.secrets.contains(word) {
                    "********".to_string()
                } else {
                    word.clone()
                }
            })
            .collect();
        match self.lines {
            Lines::Exec | Lines::Chroot | Lines::NoUser => {
                super::trace::exec_list(&self.name, &masked);
            }
            Lines::Run => super::trace::run_list(&self.name, &masked),
        }
    }

    fn trace_result(&self, captured: &Captured) {
        super::trace::result(&self.name, captured);
    }

    /// The runtime's own failures share their exit codes with the commands it runs (`docker
    /// exec` exits 1 for a container that does not exist), so they are read off the first
    /// probe instead, whose every own status is known.
    fn connection_failure(
        &self,
        _captured: &Captured,
        _offered: Option<&str>,
    ) -> Option<ConnectError> {
        None
    }

    /// A container that does not exist or is stopped: the runtime said so on stderr.
    fn probe_failure(&self, probe: &Captured) -> Option<ConnectError> {
        let said = format!("{}\n{}", probe.stderr, probe.stdout);
        if let Some((_, message)) = self.failures.iter().find(|(text, _)| said.contains(text)) {
            return Some(ConnectError::Unreachable(message.clone()));
        }
        Some(ConnectError::Unreachable(format!(
            "{} could not run a command in {}: {}",
            self.prefix[0],
            self.name,
            first_words(
                &probe.stderr,
                &first_words(&probe.stdout, "it exited without a message")
            )
        )))
    }
}
