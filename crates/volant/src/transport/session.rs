// SPDX-License-Identifier: GPL-3.0-or-later
//! Opening a link to a host over `ssh`, directly or escalated.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::agent::{AgentLink, AgentSource};

use super::bootstrap::{Captured, EXIT_SSH_FAILURE, bootstrap_probe_ran};
use super::control::{
    SSH_PROGRAM, STOP_MASTER_TIMEOUT, TIMING_ONLY, config_sets_control, dump_hostname, ssh_dump,
};
use super::defaults::{BecomeMethod, ConnectError, Escalation, SshTarget};
use super::escalation::unreachable_message;
use super::escalation::{
    RunAs, SudoForm, become_success, escalated_command, preamble, preparing_for,
};
use super::escalation::{settle_form, su_command, su_ended, su_exchange};
use super::shell::shell_word;

impl SshTarget {
    /// Opens a link, through the shared connection unless the operator's own ssh configuration
    /// already multiplexes this host, in which case theirs is used and this one stays out of it.
    pub(super) async fn connect(
        &self,
        agents: &AgentSource,
        escalation: Option<&Escalation>,
    ) -> Result<AgentLink, ConnectError> {
        let resolved = self.resolved().await;
        super::trace::established(&resolved);
        let mut link = resolved.open(agents, escalation).await?;
        link.set_tty_target(resolved.use_tty.then_some(resolved));
        Ok(link)
    }

    /// The line `ssh -tt` prints when a command's session ends, which is the end of that command's
    /// stderr: `Shared connection to <HostName> closed.` when the session rides a shared
    /// connection, `Connection to <HostName> closed.` when it does not. `<HostName>` is the
    /// machine `ssh -G` says the connection goes to, and the address written for the host when
    /// `ssh -G` cannot run. `None` when the host turns terminals off. Called on the target
    /// [`SshTarget::resolved`] returned, and only when a command is about to need the line.
    pub(crate) async fn tty_close(&self) -> Option<String> {
        if !self.use_tty {
            return None;
        }
        let (host, config_shared) = if let Some(seen) = &self.dumped {
            (**seen).clone()
        } else {
            let bare = SshTarget {
                control_path: None,
                ..self.clone()
            };
            let dump = ssh_dump(&bare).await;
            (
                dump.as_deref().and_then(dump_hostname),
                dump.as_deref().is_some_and(config_sets_control),
            )
        };
        let host = host.unwrap_or_else(|| self.address.clone());
        let shared = self.control_path.is_some() || config_shared;
        Some(format!(
            "{} to {host} closed.",
            if shared {
                "Shared connection"
            } else {
                "Connection"
            }
        ))
    }

    /// The target a link really runs, as `ssh -G` resolves it from the operator's configuration
    /// files and every option of this host, without connecting. No sharing when that already
    /// has a `ControlMaster`, `ControlPath` or `ControlPersist`, or when `ssh -G` cannot run.
    /// Otherwise the socket name also hashes the resolved configuration, so a `HostName`,
    /// `User`, `Port`, `ProxyJump` or `IdentityFile` changed in `~/.ssh/config` between two runs
    /// opens a new master rather than riding the previous run's to the old machine. Lines that
    /// only time the connection ([`TIMING_ONLY`]) are left out: a `reboot` with its own
    /// `connect_timeout` reconnects through the socket it has just stopped, not a second one
    /// whose master would still ride the connection the host dropped.
    pub(crate) async fn resolved(&self) -> SshTarget {
        let bare = SshTarget {
            control_path: None,
            ..self.clone()
        };
        let Some(path) = &self.control_path else {
            return bare;
        };
        let Some(dump) = ssh_dump(&bare).await else {
            return bare;
        };
        let dumped = Some(Box::new((dump_hostname(&dump), config_sets_control(&dump))));
        if config_sets_control(&dump) {
            return SshTarget { dumped, ..bare };
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(path.as_os_str().as_encoded_bytes());
        for line in dump.lines().filter(|line| {
            !TIMING_ONLY.contains(&line.split_once(' ').map_or(*line, |(key, _)| key))
        }) {
            hasher.update(b"\0");
            hasher.update(line.as_bytes());
        }
        let name = &hasher.finalize().to_hex()[..16];
        SshTarget {
            control_path: Some(path.with_file_name(name).into_boxed_path()),
            dumped,
            ..bare
        }
    }

    /// `ssh -O exit` on this host's shared connection, if it has one: the master exits and the
    /// next `ssh` opens a new connection. A master whose host went away without closing the
    /// connection would otherwise take every new session and leave it waiting on TCP
    /// retransmissions, which `ConnectTimeout` does not bound. The status is not read: no
    /// master is the state this asks for. The whole of it, the `ssh -G` that finds the socket
    /// included, is bounded by [`STOP_MASTER_TIMEOUT`].
    pub(super) async fn stop_master(&self) {
        let stop = async {
            let target = self.resolved().await;
            let Some(path) = &target.control_path else {
                return;
            };
            let _ = Command::new(SSH_PROGRAM)
                .args(["-O", "exit", "-o"])
                .arg(format!("ControlPath={}", path.display()))
                .arg(&target.address)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status()
                .await;
        };
        let _ = tokio::time::timeout(STOP_MASTER_TIMEOUT, stop).await;
    }

    async fn open(
        &self,
        agents: &AgentSource,
        escalation: Option<&Escalation>,
    ) -> Result<AgentLink, ConnectError> {
        // The escalation is settled first, because the bootstrap itself now runs as the target
        // user: the agent is cached under that user's own `remote_tmp`, which is the only place
        // an unprivileged `become_user` is sure to be able to read it. A home directory left at
        // mode 0700, the default on several distributions, makes the connecting user's cache
        // unreachable to anybody else, and every escalated task on the host then failed with
        // whatever the remote shell said about a file it could not reach.
        // The probe that settles the form is the bootstrap's own first probe, so its answer is
        // carried over rather than asked for twice: on a host whose agent is already cached
        // that makes an escalated link two `ssh` invocations, one fewer than before.
        if let Some(e) = escalation
            && e.method == BecomeMethod::Su
        {
            return self.open_su(agents, e).await;
        }
        let (escalated, probed) = match escalation {
            Some(e) => {
                let (form, probe) = self.check_escalation(e).await?;
                (Some((e, form)), Some(probe))
            }
            None => (None, None),
        };
        self.bootstrap(agents, RunAs::Direct(escalated), probed)
            .await
            .map_err(|err| preparing_for(err, escalation))?;
        let agent = shell_word(&self.agent_path());
        let remote = match escalated {
            None => format!("exec {agent}"),
            Some(_) => format!(
                "exec {}",
                escalated_command(escalated, &format!("exec {agent}"))
            ),
        };
        let argv = self.ssh_argv(&remote);
        super::trace::exec(self, &argv);
        let child = Command::new(SSH_PROGRAM)
            .args(&argv[1..])
            .envs(crate::askpass::environment())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ConnectError::Unreachable(format!("starting ssh: {e}")))?;
        AgentLink::new_with_preamble(child, preamble(escalated))
            .await
            .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))
    }

    /// The remote twin of `check_local_escalation`: one short `ssh` that runs the bootstrap
    /// probe as the escalated user, so the link itself only ever opens once the host has proved
    /// it will escalate, in a form the host has proved it accepts.
    ///
    /// The probe cannot ask for the agent's version the way the local one does, because on a
    /// host connected to for the first time the agent is not there yet - and it is that very
    /// upload this settles the form for. It reads the probe's exit code instead, which is
    /// proof of the same thing: only the bootstrap's own script produces one of those codes.
    async fn check_escalation(
        &self,
        escalation: &Escalation,
    ) -> Result<(SudoForm, Captured), ConnectError> {
        let command = self.probe_command();
        settle_form(escalation, bootstrap_probe_ran, |form| {
            self.run_bootstrap(RunAs::Direct(Some((escalation, form))), &command, None)
        })
        .await
    }

    /// A link whose agent `su` starts. The first probe goes through `su` on its own, so its
    /// refusals and its timeout reach the task as the reference words them; only what goes
    /// wrong after `su` has let it in is said to be about preparing the agent.
    async fn open_su(
        &self,
        agents: &AgentSource,
        escalation: &Escalation,
    ) -> Result<AgentLink, ConnectError> {
        let probe = self.run_su(escalation, &self.probe_command(), None).await?;
        self.bootstrap(agents, RunAs::Su(escalation), Some(probe))
            .await
            .map_err(|err| preparing_for(err, Some(escalation)))?;
        // The agent's stderr would land in the stream it answers on, so it goes to a file
        // beside it, which a dead link names. Its hangup at the end of the link is what the
        // agent's own `SIGHUP` handler is for.
        let agent = shell_word(&self.agent_path());
        let stderr_file = format!("{}/agent.stderr", self.cache_dir());
        let success = become_success();
        let mut child = self.spawn_su(
            escalation,
            &format!("exec {agent} 2>{}", shell_word(&stderr_file)),
            &success,
            false,
            Stdio::inherit(),
        )?;
        let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(ConnectError::Unreachable(
                "ssh was started without its pipes".to_string(),
            ));
        };
        let password = escalation.password.as_deref();
        let timeout = self.escalation_timeout();
        if let Some(said) = su_exchange(
            &mut stdout,
            &mut stdin,
            password,
            &escalation.prompt_l10n,
            &success,
            timeout,
        )
        .await?
        {
            let code = child.wait().await.ok().and_then(|status| status.code());
            return Err(su_ended(code, &said, "", password));
        }
        child.stdin = Some(stdin);
        child.stdout = Some(stdout);
        AgentLink::new_over_tty(child, stderr_file)
            .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))
    }

    /// How long `su` may stay silent before the link is given up: the connection timeout and
    /// two seconds, the reference's own measure for the same wait.
    fn escalation_timeout(&self) -> Duration {
        self.connect_timeout + Duration::from_secs(2)
    }

    /// `ssh -tt` running `command` behind `su`, with `stdin` and `stdout` piped for
    /// [`su_exchange`]. `su` reads its password from a terminal, so the remote side gets one;
    /// `-e none` keeps the agent's bytes from ever reading as an escape sequence once they
    /// travel over it, and `LogLevel=ERROR` keeps `ssh` from announcing the closed connection
    /// on the controller's stderr when the link ends.
    fn spawn_su(
        &self,
        escalation: &Escalation,
        command: &str,
        success: &str,
        compress: bool,
        stderr: Stdio,
    ) -> Result<tokio::process::Child, ConnectError> {
        if escalation.user.starts_with('-') {
            // `su` would read it as one of its own options.
            return Err(ConnectError::Become(format!(
                "'become_user' must be a user name for su, found '{}'",
                escalation.user
            )));
        }
        let mut argv = self.ssh_argv_with(&su_command(escalation, command, success), compress);
        let address = argv.len() - 3;
        argv.splice(
            address..address,
            ["-o".to_string(), "LogLevel=ERROR".into()],
        );
        argv.splice(1..1, ["-tt".to_string(), "-e".into(), "none".into()]);
        super::trace::exec(self, &argv);
        Command::new(&argv[0])
            .args(&argv[1..])
            .envs(crate::askpass::environment())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ConnectError::Unreachable(format!("starting ssh: {e}")))
    }

    /// One bootstrap command behind `su`, captured the way [`Self::run_capturing`] captures one
    /// over a pipe: what it printed once `su` let it in, its exit code, and `ssh`'s own words.
    pub(super) async fn run_su(
        &self,
        escalation: &Escalation,
        command: &str,
        payload: Option<&[u8]>,
    ) -> Result<Captured, ConnectError> {
        let success = become_success();
        let mut child = self.spawn_su(
            escalation,
            command,
            &success,
            payload.is_some(),
            Stdio::piped(),
        )?;
        let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(ConnectError::Unreachable(
                "ssh was started without its pipes".to_string(),
            ));
        };
        let stderr = tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let password = escalation.password.as_deref();
        let timeout = self.escalation_timeout();
        if let Some(said) = su_exchange(
            &mut stdout,
            &mut stdin,
            password,
            &escalation.prompt_l10n,
            &success,
            timeout,
        )
        .await?
        {
            let code = child.wait().await.ok().and_then(|status| status.code());
            let stderr = stderr.await.unwrap_or_default();
            return Err(su_ended(code, &said, &stderr, password));
        }
        if let Some(bytes) = payload {
            // A command that gives up early closes the terminal; its exit code says why.
            let _ = stdin.write_all(bytes).await;
            let _ = stdin.flush().await;
        }
        drop(stdin);
        let mut out = Vec::new();
        stdout
            .read_to_end(&mut out)
            .await
            .map_err(|e| ConnectError::Unreachable(format!("reading ssh: {e}")))?;
        let status = child
            .wait()
            .await
            .map_err(|e| ConnectError::Unreachable(format!("waiting for ssh: {e}")))?;
        let captured = Captured {
            code: status.code(),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: stderr.await.unwrap_or_default(),
        };
        if captured.code == Some(EXIT_SSH_FAILURE) {
            return Err(ConnectError::Unreachable(unreachable_message(
                &captured.stderr,
                password,
            )));
        }
        Ok(captured)
    }
}
