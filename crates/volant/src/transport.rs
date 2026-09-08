// SPDX-License-Identifier: GPL-3.0-or-later
//! How the controller reaches a host. Every transport ends up as a process whose stdin and
//! stdout carry protocol frames: the agent itself for `local`, an `ssh` running the agent
//! remotely for `ssh`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::bail;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::agent::{AgentLink, AgentSource};
use crate::inventory::Host;

/// Connection settings from `ansible.cfg` and the command line; host variables override them.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionDefaults {
    pub remote_user: Option<String>,
    pub private_key: Option<PathBuf>,
    pub host_key_checking: bool,
    pub remote_tmp: String,
    pub connect_timeout: Duration,
    /// Whether a task escalates when neither the play nor the host says otherwise.
    pub r#become: bool,
    /// Who to become, Ansible's `become_user`, `root` unless configured otherwise.
    pub become_user: String,
    /// Only `sudo` is implemented; anything else is refused before a playbook runs.
    pub become_method: String,
    /// The `--ask-become-pass` answer, if one was asked for.
    pub become_password: Option<String>,
}

/// Written by hand so `become_password` cannot reach a log or a `-vvv` dump: a derived `Debug`
/// on this struct would print the operator's password every time the connection defaults are
/// formatted.
impl std::fmt::Debug for ConnectionDefaults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionDefaults")
            .field("remote_user", &self.remote_user)
            .field("private_key", &self.private_key)
            .field("host_key_checking", &self.host_key_checking)
            .field("remote_tmp", &self.remote_tmp)
            .field("connect_timeout", &self.connect_timeout)
            .field("become", &self.r#become)
            .field("become_user", &self.become_user)
            .field("become_method", &self.become_method)
            .field("become_password", &redacted(&self.become_password))
            .finish()
    }
}

/// Privilege escalation for one link: run the agent as `user` through `sudo`.
#[derive(Clone, PartialEq, Eq)]
pub struct Escalation {
    pub user: String,
    pub password: Option<String>,
}

/// Also written by hand, and for the same reason: `Escalation` travels through the executor,
/// where a derived `Debug` would put the password in any diagnostic that formats a batch.
impl std::fmt::Debug for Escalation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Escalation")
            .field("user", &self.user)
            .field("password", &redacted(&self.password))
            .finish()
    }
}

fn redacted(password: &Option<String>) -> &'static str {
    match password {
        Some(_) => "<redacted>",
        None => "None",
    }
}

/// What a run says when `sudo` wants a password and none was given, and when the one given was
/// wrong. Both texts are ansible-core 2.19.12's own, measured against `sudo` 1.9.15p5.
pub const MISSING_SUDO_PASSWORD: &str = "Missing sudo password";
pub const INCORRECT_SUDO_PASSWORD: &str = "Incorrect sudo password";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// Run the agent on the controller machine itself.
    Local,
    Ssh(SshTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub address: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub private_key: Option<PathBuf>,
    pub common_args: Vec<String>,
    pub extra_args: Vec<String>,
    pub host_key_checking: bool,
    pub connect_timeout: Duration,
    /// Where the agent is cached on the host, `~` expanded by the remote shell.
    pub remote_tmp: String,
}

/// Why a link could not be opened. `Unreachable` is Ansible's `UNREACHABLE`; `Become` is a
/// host that answered perfectly well and then refused to escalate, which is a failed task and
/// not an unreachable host.
#[derive(Debug)]
pub enum ConnectError {
    Unreachable(String),
    Become(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Unreachable(msg) | ConnectError::Become(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Exit codes the bootstrap shell uses to talk back before any frame exists. They are picked
/// above every code the shell itself hands out (126, 127) and below 255, which belongs to
/// `ssh`, so a code seen by `run_capturing` names exactly one situation.
///
/// Every command the bootstrap runs replaces the status of the programs it calls with one of
/// these, so none of them can hand back 255 and be read as `ssh`'s own failure: the cached
/// agent's status becomes `EXIT_AGENT_UNRUNNABLE` and `uname`'s becomes `EXIT_UNAME_FAILED`.
const EXIT_AGENT_MISSING: i32 = 42;
const EXIT_NO_SPACE: i32 = 43;
const EXIT_SHORT_WRITE: i32 = 44;
const EXIT_AGENT_UNRUNNABLE: i32 = 45;
const EXIT_UNAME_FAILED: i32 = 46;
/// `ssh` reports its own failures with this, whatever the remote command would have returned.
const EXIT_SSH_FAILURE: i32 = 255;

impl Transport {
    pub fn for_host(host: &Host, defaults: &ConnectionDefaults) -> anyhow::Result<Transport> {
        let text = |key: &str| {
            host.vars
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        match text("ansible_connection").as_deref().unwrap_or("ssh") {
            "local" => Ok(Transport::Local),
            "ssh" => Ok(Transport::Ssh(SshTarget {
                address: text("ansible_host").unwrap_or_else(|| host.name.clone()),
                port: port_of(host),
                user: text("ansible_user").or_else(|| defaults.remote_user.clone()),
                private_key: text("ansible_ssh_private_key_file")
                    .map(PathBuf::from)
                    .or_else(|| defaults.private_key.clone()),
                common_args: split_args(host, "ansible_ssh_common_args")?,
                extra_args: split_args(host, "ansible_ssh_extra_args")?,
                host_key_checking: defaults.host_key_checking,
                connect_timeout: defaults.connect_timeout,
                remote_tmp: text("ansible_remote_tmp")
                    .unwrap_or_else(|| defaults.remote_tmp.clone()),
            })),
            other => bail!(
                "host '{}': connection '{other}' is not supported",
                host.name
            ),
        }
    }

    pub async fn connect(
        &self,
        agents: &AgentSource,
        escalation: Option<&Escalation>,
    ) -> Result<AgentLink, ConnectError> {
        match self {
            Transport::Local => {
                let agent = agents
                    .local()
                    .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))?;
                let agent = agent.display().to_string();
                if let Some(escalation) = escalation {
                    check_local_escalation(&agent, escalation).await?;
                }
                let argv = local_argv(&agent, escalation);
                let child = Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| {
                        ConnectError::Unreachable(format!("starting agent {agent}: {e}"))
                    })?;
                AgentLink::new_with_preamble(child, preamble(escalation))
                    .await
                    .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))
            }
            Transport::Ssh(target) => target.connect(agents, escalation).await,
        }
    }
}

/// The `sudo` password, as the bytes written to the child before any frame. Built here and
/// nowhere else, so the only thing that ever holds it is a `Vec<u8>` on its way to a pipe.
fn preamble(escalation: Option<&Escalation>) -> Option<Vec<u8>> {
    let password = escalation?.password.as_ref()?;
    let mut bytes = Vec::with_capacity(password.len() + 1);
    bytes.extend_from_slice(password.as_bytes());
    bytes.push(b'\n');
    Some(bytes)
}

/// The `sudo` options that decide how the password is obtained, as separate words.
///
/// Without a password, `-n` refuses to ask, so a host that wants one fails immediately instead
/// of blocking on a prompt nothing will ever answer.
///
/// With one, `-k` is not optional. Measured on `sudo` 1.9.15p5 and `sudo-rs` 0.2.13: while a
/// previous authentication is still cached, `-S` does not read stdin at all, and the password
/// written there would be read by the agent as the first bytes of its first frame. `-k` drops
/// that cached authentication, so `-S` always consumes exactly the one line written for it.
fn sudo_auth_flags(escalation: &Escalation) -> &'static [&'static str] {
    match escalation.password {
        Some(_) => &["-k", "-S", "-p", ""],
        None => &["-n"],
    }
}

/// The command line that starts a local agent, under `sudo` when escalation is asked for.
fn local_argv(agent: &str, escalation: Option<&Escalation>) -> Vec<String> {
    let Some(escalation) = escalation else {
        return vec![agent.to_string()];
    };
    let mut argv = vec!["sudo".to_string(), "-H".to_string()];
    argv.extend(sudo_auth_flags(escalation).iter().map(|s| s.to_string()));
    argv.extend([
        "-u".to_string(),
        escalation.user.clone(),
        "--".to_string(),
        agent.to_string(),
    ]);
    argv
}

/// `sudo -H ... --`, up to but not including the agent, as shell words for a remote command.
/// Empty without escalation, so one `format!` covers both cases at every call site.
fn sudo_prefix(escalation: Option<&Escalation>) -> String {
    let Some(escalation) = escalation else {
        return String::new();
    };
    let flags = sudo_auth_flags(escalation)
        .iter()
        .map(|f| single_quoted(f))
        .collect::<Vec<_>>()
        .join(" ");
    format!("sudo -H {flags} -u {} -- ", single_quoted(&escalation.user))
}

/// Asks `sudo` to print the agent's version as the escalated user, before the link itself is
/// opened. A refusal is diagnosed here, from `sudo`'s own words, rather than turning up later
/// as an agent that never answered; and because the agent has not started yet, nothing has to
/// read a pipe the running agent also writes to.
async fn check_local_escalation(agent: &str, escalation: &Escalation) -> Result<(), ConnectError> {
    let mut argv = local_argv(agent, Some(escalation));
    argv.push("--version".to_string());
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        // The messages matched below are `sudo`'s English ones.
        .env("LC_ALL", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(match escalation.password {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        });
    let mut child = command
        .spawn()
        .map_err(|e| ConnectError::Become(format!("starting sudo: {e}")))?;
    if let (Some(bytes), Some(mut pipe)) = (preamble(Some(escalation)), child.stdin.take()) {
        // A `sudo` that refuses before reading closes the pipe; its own words below say what
        // happened, so a broken pipe here is not the error worth reporting.
        let _ = pipe.write_all(&bytes).await;
        drop(pipe);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| ConnectError::Become(format!("waiting for sudo: {e}")))?;
    escalation_outcome(
        output.status.code(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
        escalation.password.is_some(),
    )
}

/// Whether `sudo` really did start the agent as the asked-for user, or why it did not.
///
/// Success is the agent's own version line, not `sudo`'s exit status alone: a `sudo` that
/// exited zero having run something else is not an escalation that worked.
///
/// The strings are the two `sudo` implementations' own, measured on the development machine
/// (`sudo-rs` 0.2.13) and on the target machine (`sudo` 1.9.15p5); a host may run either, so
/// both wordings are matched. Which of the two messages a match produces is decided by whether
/// this link had a password to offer rather than by the text, whose split between "none given"
/// and "wrong one" differs between the two implementations.
fn escalation_outcome(
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
    had_password: bool,
) -> Result<(), ConnectError> {
    if code == Some(0) && stdout.trim() == format!("volant-agent {}", env!("CARGO_PKG_VERSION")) {
        return Ok(());
    }
    const AUTHENTICATION: &[&str] = &[
        "a password is required",
        "interactive authentication is required",
        "no password was provided",
        "Sorry, try again",
        "incorrect password",
        "Authentication failed",
        "Authentication required but not attempted",
    ];
    if AUTHENTICATION.iter().any(|m| stderr.contains(m)) {
        return Err(ConnectError::Become(
            if had_password {
                INCORRECT_SUDO_PASSWORD
            } else {
                MISSING_SUDO_PASSWORD
            }
            .to_string(),
        ));
    }
    Err(ConnectError::Become(first_words(
        stderr,
        "sudo refused without a message",
    )))
}

/// The words of one `ansible_ssh_*_args` variable. An unbalanced quote is refused by name
/// rather than dropped: silently connecting without a `ProxyJump` or `ProxyCommand` the
/// inventory asked for can reach a different machine than the operator meant.
fn split_args(host: &Host, key: &str) -> anyhow::Result<Vec<String>> {
    let Some(text) = host.vars.get(key).and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    shlex::split(text).ok_or_else(|| {
        anyhow::anyhow!(
            "host '{}': {key} has unbalanced quotes and cannot be turned into ssh options: {text}",
            host.name
        )
    })
}

/// One remote path as a single shell word. A leading `~` or `~user` stays bare, up to and
/// including the first `/`, so the remote shell, the only thing that can, expands it; the rest
/// is single-quoted, so a `remote_tmp` holding a space or a shell metacharacter cannot split
/// into several words or run anything.
fn shell_word(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return single_quoted(path);
    };
    match rest.find('/') {
        Some(slash) => format!("~{}/{}", &rest[..slash], single_quoted(&rest[slash + 1..])),
        // Just `~` or `~user`, with nothing after it to hold a space or a metacharacter.
        None => path.to_string(),
    }
}

fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `ansible_port`, whether the inventory typed it as a number or quoted it as a string.
fn port_of(host: &Host) -> Option<u16> {
    let value = host.vars.get("ansible_port")?;
    match value {
        Value::Number(_) => value.as_u64().and_then(|p| u16::try_from(p).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl SshTarget {
    /// The `ssh` command line for one remote command. Options first, then the host, then `--`.
    pub fn ssh_argv(&self, remote_command: &str) -> Vec<String> {
        self.ssh_argv_with(remote_command, false)
    }

    /// The same, with `-C` when the command carries a payload on stdin. Compression is an
    /// option of `ssh`, so it belongs before the address: everything from the address onwards
    /// is the remote command's own argument list, and an option appended there would reach
    /// the remote shell instead.
    pub fn ssh_argv_with(&self, remote_command: &str, compress: bool) -> Vec<String> {
        let mut argv = vec!["ssh".to_string()];
        if compress {
            argv.push("-C".into());
        }
        argv.extend([
            "-o".to_string(),
            "BatchMode=yes".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout.as_secs()),
        ]);
        if !self.host_key_checking {
            argv.extend([
                "-o".into(),
                "StrictHostKeyChecking=no".into(),
                "-o".into(),
                "UserKnownHostsFile=/dev/null".into(),
            ]);
        }
        if let Some(port) = self.port {
            argv.extend(["-p".into(), port.to_string()]);
        }
        if let Some(key) = &self.private_key {
            argv.extend(["-i".into(), key.display().to_string()]);
        }
        argv.extend(self.common_args.iter().cloned());
        argv.extend(self.extra_args.iter().cloned());
        if let Some(user) = &self.user {
            argv.extend(["-l".into(), user.clone()]);
        }
        argv.extend([
            self.address.clone(),
            "--".into(),
            remote_command.to_string(),
        ]);
        argv
    }

    fn cache_dir(&self) -> String {
        format!(
            "{}/volant-agent-{}",
            self.remote_tmp,
            env!("CARGO_PKG_VERSION")
        )
    }

    /// Where the agent itself sits on the host, as this controller writes it in a message.
    fn agent_path(&self) -> String {
        format!("{}/volant-agent", self.cache_dir())
    }

    /// Prints the cached agent's version, or the machine architecture followed by exit 42. A
    /// cached agent that cannot run exits 45 and `uname` failing exits 46, so neither hands
    /// its own status back to `ssh`, where 255 would read as a connection failure.
    pub fn probe_command(&self) -> String {
        format!(
            "a={agent}; if [ -x \"$a\" ]; then \"$a\" --version || {{ echo \"exit status $?\" >&2; exit {EXIT_AGENT_UNRUNNABLE}; }}; else uname -m || exit {EXIT_UNAME_FAILED}; exit {EXIT_AGENT_MISSING}; fi",
            agent = shell_word(&self.agent_path())
        )
    }

    /// Reads the agent from stdin into the cache, atomically, after checking free space. The
    /// byte count is verified before the temporary copy is made executable and renamed, so a
    /// transfer cut short never lands under the final name; older cached versions are removed
    /// afterwards, and failing to remove one does not fail an upload that has already landed.
    /// The temporary name carries the remote shell's pid, so two uploads racing on one host
    /// write to separate files instead of interleaving into one of exactly the right size. A
    /// connection killed mid-transfer still orphans its own `.tmp.<pid>` file, since nothing
    /// left running can clean it up; an hour-old sweep after a successful `mv` removes those
    /// without ever touching a sibling upload still in flight.
    pub fn upload_command(&self, size: u64) -> String {
        format!(
            "d={dir}; mkdir -p \"$d\" || exit 1; \
             t=\"$d/volant-agent.tmp.$$\"; \
             avail=$(df -Pk \"$d\" | awk 'NR==2 {{print $4}}'); \
             case \"$avail\" in ''|*[!0-9]*) avail=0;; esac; \
             [ \"$avail\" -ge {need} ] || {{ echo \"$avail\"; exit {EXIT_NO_SPACE}; }}; \
             cat > \"$t\" || {{ rm -f \"$t\"; exit 1; }}; \
             [ \"$(wc -c < \"$t\")\" -eq {size} ] || {{ rm -f \"$t\"; exit {EXIT_SHORT_WRITE}; }}; \
             chmod 755 \"$t\" || {{ rm -f \"$t\"; exit 1; }}; \
             mv \"$t\" \"$d/volant-agent\" || {{ rm -f \"$t\"; exit 1; }}; \
             find \"$d\" -name 'volant-agent.tmp.*' -mmin +60 -delete 2>/dev/null; \
             for old in {tmp}/volant-agent-*; do [ \"$old\" = \"$d\" ] || rm -rf \"$old\"; done; \
             exit 0",
            dir = shell_word(&self.cache_dir()),
            need = space_needed_kib(size),
            tmp = shell_word(&self.remote_tmp),
        )
    }

    async fn connect(
        &self,
        agents: &AgentSource,
        escalation: Option<&Escalation>,
    ) -> Result<AgentLink, ConnectError> {
        self.bootstrap(agents).await?;
        if let Some(escalation) = escalation {
            self.check_escalation(escalation).await?;
        }
        let remote = format!(
            "exec {}{}",
            sudo_prefix(escalation),
            shell_word(&self.agent_path())
        );
        let child = Command::new("ssh")
            .args(&self.ssh_argv(&remote)[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ConnectError::Unreachable(format!("starting ssh: {e}")))?;
        AgentLink::new_with_preamble(child, preamble(escalation))
            .await
            .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))
    }

    /// The remote twin of `check_local_escalation`: one short `ssh` that asks `sudo` for the
    /// agent's version as the escalated user, so the link itself only ever opens once the host
    /// has proved it will escalate.
    async fn check_escalation(&self, escalation: &Escalation) -> Result<(), ConnectError> {
        let command = format!(
            "LC_ALL=C {}{} --version",
            sudo_prefix(Some(escalation)),
            shell_word(&self.agent_path())
        );
        let captured = self
            .run_capturing(&command, preamble(Some(escalation)).as_deref())
            .await?;
        escalation_outcome(
            captured.code,
            &captured.stdout,
            &captured.stderr,
            escalation.password.is_some(),
        )
    }

    /// Makes sure the host has this exact agent version cached, uploading it if it does not.
    /// Every way out other than `Ok(())` is a `ConnectError`, so no caller can go on to run a
    /// missing, stale or wrong-architecture agent.
    async fn bootstrap(&self, agents: &AgentSource) -> Result<(), ConnectError> {
        let expected = format!("volant-agent {}", env!("CARGO_PKG_VERSION"));
        let probe = self.run_capturing(&self.probe_command(), None).await?;
        if probe.code == Some(0) && probe.stdout.trim() == expected {
            return Ok(());
        }
        // Exit 42 means the probe itself read the architecture off the machine. A cached agent
        // that cannot run (45) takes the same fallback path as a wrong version: this
        // controller wrote that file with a verified byte count and an atomic rename, but a
        // truncated or `ENOEXEC` file it left behind before that check existed, or after a
        // hard-killed run, wears the same symptom, and it used to heal on the next run. So
        // exit 45 gets one more upload before it is reported; the post-upload re-probe is what
        // errors if a fresh copy still cannot run. Anything else is a remote shell that failed
        // outright, so its stdout cannot be read as a version string. Ask the machine again,
        // separately.
        let arch = match probe.code {
            Some(EXIT_AGENT_MISSING) => probe.stdout.trim().to_string(),
            Some(EXIT_UNAME_FAILED) => {
                return Err(ConnectError::Unreachable(format!(
                    "the remote shell failed: {}",
                    first_words(&probe.stderr, "'uname -m' said nothing")
                )));
            }
            _ => {
                let uname = self
                    .run_capturing(&format!("uname -m || exit {EXIT_UNAME_FAILED}"), None)
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
        let local = agents.for_target(triple).ok_or_else(|| {
            ConnectError::Unreachable(format!(
                "no agent binary for {arch} (looked for volant-agent-{triple} in {})",
                agents.describe()
            ))
        })?;
        let bytes = std::fs::read(&local)
            .map_err(|e| ConnectError::Unreachable(format!("reading {}: {e}", local.display())))?;
        let size = bytes.len() as u64;
        let upload = self
            .run_capturing(&self.upload_command(size), Some(&bytes))
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
        let check = self.run_capturing(&self.probe_command(), None).await?;
        if check.code == Some(EXIT_AGENT_UNRUNNABLE) {
            return Err(ConnectError::Unreachable(format!(
                "the cached agent {} cannot be run: {}",
                self.agent_path(),
                first_words(&check.stderr, "it failed without a message")
            )));
        }
        if check.code != Some(0) || check.stdout.trim() != expected {
            return Err(ConnectError::Unreachable(
                "agent version mismatch after upload".to_string(),
            ));
        }
        Ok(())
    }

    /// Runs one remote command to completion, feeding `stdin` if given, and returns its
    /// output. An `ssh` exit of 255 is a connection failure, reported with ssh's own words.
    async fn run_capturing(
        &self,
        remote_command: &str,
        stdin: Option<&[u8]>,
    ) -> Result<Captured, ConnectError> {
        let argv = self.ssh_argv_with(remote_command, stdin.is_some());
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
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
        // `ssh` reports its own failures as 255 and none of the bootstrap commands ever
        // returns it, so this cannot swallow a remote command's own status.
        if captured.code == Some(EXIT_SSH_FAILURE) {
            return Err(ConnectError::Unreachable(format!(
                "Failed to connect to the host via ssh: {}",
                first_words(&captured.stderr, "ssh failed without a message")
            )));
        }
        Ok(captured)
    }
}

struct Captured {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// What a stream said, or `fallback` when it said nothing worth printing.
fn first_words(text: &str, fallback: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn host(vars: serde_json::Value) -> Host {
        Host {
            name: "web1".into(),
            vars: vars
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    fn defaults() -> ConnectionDefaults {
        ConnectionDefaults {
            remote_user: None,
            private_key: None,
            host_key_checking: true,
            remote_tmp: "~/.ansible/tmp".into(),
            connect_timeout: Duration::from_secs(10),
            r#become: false,
            become_user: "root".into(),
            become_method: "sudo".into(),
            become_password: None,
        }
    }

    fn escalation(password: Option<&str>) -> Escalation {
        Escalation {
            user: "deploy".into(),
            password: password.map(str::to_string),
        }
    }

    /// Without a password `-n` refuses to ask, so a host that wants one fails fast. With one,
    /// `-k` is what makes `-S` read stdin at all: measured on `sudo` 1.9.15p5 and `sudo-rs`
    /// 0.2.13, a still-valid authentication makes `-S` leave the password on the pipe, where
    /// the agent would read it as the first bytes of its first frame.
    #[test]
    fn the_sudo_command_line_names_the_user_and_asks_for_the_password_on_stdin() {
        let argv = local_argv("/usr/bin/volant-agent", Some(&escalation(None)));
        assert_eq!(
            argv,
            [
                "sudo",
                "-H",
                "-n",
                "-u",
                "deploy",
                "--",
                "/usr/bin/volant-agent"
            ]
        );
        let argv = local_argv("/usr/bin/volant-agent", Some(&escalation(Some("s3cret"))));
        assert_eq!(
            argv,
            [
                "sudo",
                "-H",
                "-k",
                "-S",
                "-p",
                "",
                "-u",
                "deploy",
                "--",
                "/usr/bin/volant-agent"
            ]
        );
        assert!(
            !argv.contains(&"s3cret".to_string()),
            "the password never reaches a command line: {argv:?}"
        );
        assert_eq!(
            local_argv("/usr/bin/volant-agent", None),
            ["/usr/bin/volant-agent"],
            "without escalation the agent is the command"
        );
    }

    #[test]
    fn the_remote_sudo_prefix_quotes_the_user_and_carries_no_password() {
        let prefix = sudo_prefix(Some(&escalation(Some("s3cret"))));
        assert_eq!(prefix, "sudo -H '-k' '-S' '-p' '' -u 'deploy' -- ");
        assert!(!prefix.contains("s3cret"), "{prefix}");
        assert_eq!(
            sudo_prefix(Some(&Escalation {
                user: "it's me".into(),
                password: None
            })),
            r"sudo -H '-n' -u 'it'\''s me' -- ",
            "a user name that would otherwise break out of the remote shell word is quoted"
        );
        assert!(sudo_prefix(None).is_empty());
    }

    /// The strings come from the two `sudo` implementations measured for this: `sudo` 1.9.15p5
    /// on the target machine and `sudo-rs` 0.2.13 on the development machine. Which message a
    /// match produces is decided by whether a password was offered, because the two
    /// implementations split their wording between the two cases differently.
    #[test]
    fn sudos_refusals_are_told_apart_by_whether_a_password_was_offered() {
        let outcome = |stderr: &str, had_password| match escalation_outcome(
            Some(1),
            "",
            stderr,
            had_password,
        ) {
            Err(ConnectError::Become(msg)) => msg,
            other => panic!("expected Become, got {other:?}"),
        };
        for stderr in [
            "sudo: a password is required",
            "sudo: interactive authentication is required",
        ] {
            assert_eq!(outcome(stderr, false), MISSING_SUDO_PASSWORD, "{stderr}");
        }
        for stderr in [
            "Sorry, try again.\n\nsudo: no password was provided",
            "sudo: Authentication failed, try again.",
            "sudo: Authentication required but not attempted",
            "sudo: 1 incorrect password attempt",
        ] {
            assert_eq!(outcome(stderr, true), INCORRECT_SUDO_PASSWORD, "{stderr}");
        }
        assert_eq!(
            outcome("sh: 1: sudo: not found", false),
            "sh: 1: sudo: not found",
            "a host without sudo gets the shell's own words, not a guess about passwords"
        );
        assert_eq!(
            outcome("", false),
            "sudo refused without a message",
            "a silent refusal still says something"
        );
    }

    /// A `sudo` that exits zero having run something else is not an escalation that worked.
    /// Checking the agent's own version line is what keeps a wrapper, an alias or a stale
    /// cached binary from being read as a success and then running the task as the wrong user.
    #[test]
    fn escalation_succeeds_only_on_the_agents_own_version_line() {
        let version = format!("volant-agent {}", env!("CARGO_PKG_VERSION"));
        assert!(escalation_outcome(Some(0), &version, "", false).is_ok());
        assert!(
            escalation_outcome(Some(0), &format!("{version}\n"), "", false).is_ok(),
            "a trailing newline is not a different version"
        );
        assert!(
            escalation_outcome(Some(0), "volant-agent 0.0.0-stale", "", false).is_err(),
            "another version is not this agent"
        );
        assert!(
            escalation_outcome(Some(0), "", "", false).is_err(),
            "a zero exit with nothing printed proves nothing"
        );
    }

    #[test]
    fn ssh_is_the_default_connection_and_reads_the_ansible_variables() {
        let t = Transport::for_host(
            &host(json!({
                "ansible_host": "10.0.0.5", "ansible_port": 2222, "ansible_user": "deploy",
                "ansible_ssh_private_key_file": "/keys/id", "ansible_ssh_common_args": "-o ProxyJump=bastion",
                "ansible_ssh_extra_args": "-vv", "ansible_remote_tmp": "/var/tmp/v"
            })),
            &defaults(),
        )
        .unwrap();
        let Transport::Ssh(target) = t else {
            panic!("expected ssh")
        };
        let argv = target.ssh_argv("true");
        let text = argv.join(" ");
        assert!(
            text.starts_with("ssh -o BatchMode=yes -o ConnectTimeout=10"),
            "{text}"
        );
        assert!(
            text.contains("-p 2222") && text.contains("-i /keys/id") && text.contains("-l deploy"),
            "{text}"
        );
        assert!(
            text.contains("-o ProxyJump=bastion -vv"),
            "common args come before extra args: {text}"
        );
        assert!(text.ends_with("10.0.0.5 -- true"), "{text}");
        assert!(
            !text.contains("StrictHostKeyChecking"),
            "checking on means ssh defaults: {text}"
        );
        assert_eq!(target.remote_tmp, "/var/tmp/v");
    }

    #[test]
    fn host_key_checking_off_adds_the_two_options() {
        let mut d = defaults();
        d.host_key_checking = false;
        let Transport::Ssh(target) = Transport::for_host(&host(json!({})), &d).unwrap() else {
            panic!()
        };
        let text = target.ssh_argv("true").join(" ");
        assert!(
            text.contains("-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"),
            "{text}"
        );
        assert!(
            text.ends_with("web1 -- true"),
            "the inventory name is the address by default: {text}"
        );
    }

    #[test]
    fn defaults_apply_when_the_host_says_nothing() {
        let d = ConnectionDefaults {
            remote_user: Some("ops".into()),
            private_key: Some("/k".into()),
            ..defaults()
        };
        let Transport::Ssh(target) = Transport::for_host(&host(json!({})), &d).unwrap() else {
            panic!()
        };
        let text = target.ssh_argv("true").join(" ");
        assert!(text.contains("-l ops") && text.contains("-i /k"), "{text}");
        let Transport::Ssh(target) =
            Transport::for_host(&host(json!({"ansible_user": "me"})), &d).unwrap()
        else {
            panic!()
        };
        assert!(
            target.ssh_argv("true").join(" ").contains("-l me"),
            "host vars beat defaults"
        );
    }

    #[test]
    fn a_quoted_port_is_still_a_port() {
        let Transport::Ssh(target) =
            Transport::for_host(&host(json!({"ansible_port": "2222"})), &defaults()).unwrap()
        else {
            panic!()
        };
        assert_eq!(target.port, Some(2222));
    }

    /// `run_capturing` builds its command line with this exact call, so the assertions below
    /// are about the production path and not about a vector the test filled in itself.
    #[test]
    fn compression_is_an_option_not_an_argument_of_the_remote_command() {
        let Transport::Ssh(target) = Transport::for_host(&host(json!({})), &defaults()).unwrap()
        else {
            panic!()
        };
        let argv = target.ssh_argv_with("cat > f", true);
        assert_eq!(argv[1], "-C", "{argv:?}");
        assert!(
            argv.join(" ").ends_with("web1 -- cat > f"),
            "nothing may follow the remote command: {argv:?}"
        );
        assert!(
            !target
                .ssh_argv_with("true", false)
                .contains(&"-C".to_string()),
            "a command with no payload is not compressed"
        );
    }

    #[test]
    fn unbalanced_quotes_in_the_ssh_arguments_refuse_the_host() {
        for key in ["ansible_ssh_common_args", "ansible_ssh_extra_args"] {
            let err = Transport::for_host(
                &host(json!({ key: "-o ProxyCommand='ssh bastion" })),
                &defaults(),
            )
            .unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains(key), "the variable is named: {text}");
            assert!(text.contains("ProxyCommand"), "the value is quoted: {text}");
        }
    }

    #[test]
    fn the_bootstrap_probe_and_upload_commands_are_shaped_for_a_posix_shell() {
        let Transport::Ssh(target) = Transport::for_host(&host(json!({})), &defaults()).unwrap()
        else {
            panic!()
        };
        let probe = target.probe_command();
        assert!(
            probe.contains(&format!("volant-agent-{}", env!("CARGO_PKG_VERSION"))),
            "{probe}"
        );
        assert!(
            probe.contains("--version") && probe.contains("uname -m") && probe.contains("exit 42"),
            "{probe}"
        );
        assert!(
            probe.contains("exit 45"),
            "a cached agent that cannot run has its own code, so its status never reaches ssh: {probe}"
        );
        assert!(
            probe.contains("uname -m || exit 46"),
            "a uname that fails must not be read as an architecture: {probe}"
        );
        let upload = target.upload_command(1_000_000);
        assert!(
            upload.contains("df -Pk") && upload.contains("exit 43"),
            "{upload}"
        );
        assert!(
            upload.contains(&format!("-ge {}", space_needed_kib(1_000_000))),
            "{upload}"
        );
        assert!(
            upload.contains("chmod 755") && upload.contains("mv "),
            "{upload}"
        );
        assert!(
            upload.contains("-eq 1000000") && upload.contains("exit 44"),
            "a transfer cut short must not reach the final name: {upload}"
        );
        assert!(
            upload.find("wc -c").unwrap() < upload.find("chmod 755").unwrap(),
            "the size is checked before the file becomes executable: {upload}"
        );
        assert!(
            upload.contains("-name 'volant-agent.tmp.*' -mmin +60 -delete"),
            "an orphaned temporary from a killed connection is swept, never a live sibling: {upload}"
        );
        assert!(
            upload.find("mv ").unwrap() < upload.find("-mmin +60").unwrap(),
            "the sweep runs after this upload's own file has already been renamed away: {upload}"
        );
    }

    /// Run against a real `sh`, because the failure this guards against is a shell parsing
    /// one: an unquoted `remote_tmp` holding a space makes `mkdir -p ""` fail and the run then
    /// reports a failed upload for a path it never tried. The uploaded payload exits 255 when
    /// it is run, which is `ssh`'s own code for a connection failure, so the probe that finds
    /// it has to come back as 45 rather than letting that status through.
    #[cfg(unix)]
    #[test]
    fn a_remote_tmp_with_a_space_survives_the_shell() {
        use std::io::Write;
        use std::process::{Command as SyncCommand, Stdio};

        let base = std::env::temp_dir().join(format!("volant quoting {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let Transport::Ssh(target) = Transport::for_host(
            &host(json!({ "ansible_remote_tmp": base.to_str().unwrap() })),
            &defaults(),
        )
        .unwrap() else {
            panic!()
        };

        let payload = b"#!/bin/sh\nexit 255\n";
        let mut child = SyncCommand::new("sh")
            .arg("-c")
            .arg(target.upload_command(payload.len() as u64))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(payload).unwrap();
        let upload = child.wait_with_output().unwrap();
        assert_eq!(
            upload.status.code(),
            Some(0),
            "upload: {}",
            String::from_utf8_lossy(&upload.stderr)
        );
        let landed = base
            .join(format!("volant-agent-{}", env!("CARGO_PKG_VERSION")))
            .join("volant-agent");
        assert_eq!(std::fs::read(&landed).unwrap(), payload, "{landed:?}");

        let probe = SyncCommand::new("sh")
            .arg("-c")
            .arg(target.probe_command())
            .output()
            .unwrap();
        assert_eq!(
            probe.status.code(),
            Some(EXIT_AGENT_UNRUNNABLE),
            "a cached agent exiting 255 must not look like a connection failure: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn shell_word_expands_only_the_tilde_prefix() {
        assert_eq!(shell_word("~/a b/c"), "~/'a b/c'");
        assert_eq!(shell_word("~"), "~");
        assert_eq!(
            shell_word("~deploy/a b"),
            "~deploy/'a b'",
            "a named user's home expands too, not just the caller's own"
        );
        assert_eq!(shell_word("/var/tmp/a b"), "'/var/tmp/a b'");
        assert_eq!(
            shell_word("~/it's here"),
            r"~/'it'\''s here'",
            "an embedded single quote in the quoted remainder is escaped"
        );
    }

    #[test]
    fn remote_architectures_map_to_agent_triples() {
        assert_eq!(triple_for("x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(triple_for("aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(triple_for("arm64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(triple_for("riscv64"), None);
    }

    /// The numbers are spelled out rather than recomputed from the formula. Restating the
    /// formula in the test catches an implementation that drifts away from it, but never a
    /// formula that was wrong to begin with, which is the mistake worth guarding against here.
    #[test]
    fn space_needed_has_headroom() {
        // 672480 bytes is 656.7 KiB; two copies are 1313 KiB, plus a mebibyte of headroom.
        assert_eq!(space_needed_kib(672_480), 2337);
        assert_eq!(space_needed_kib(0), 1024, "the headroom stands on its own");
        assert_eq!(
            space_needed_kib(1_048_576),
            3072,
            "a mebibyte agent needs three"
        );
        assert!(
            space_needed_kib(5_000_000) > 2 * 5_000_000 / 1024,
            "always more than the two copies it covers"
        );
    }

    #[test]
    fn unknown_connections_are_still_refused_by_name() {
        let err = Transport::for_host(
            &host(json!({"ansible_connection": "carrier_pigeon"})),
            &defaults(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("carrier_pigeon"));
    }
}
