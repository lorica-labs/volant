// SPDX-License-Identifier: GPL-3.0-or-later
//! How the controller reaches a host. Every transport ends up as a process whose stdin and
//! stdout carry protocol frames: the agent itself for `local`, an `ssh` running the agent
//! remotely for `ssh`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::bail;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::agent::{AgentLink, AgentSource};
use crate::inventory::Host;
use crate::vars::host_setting;

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
            .field("become_password", &redacted(self.become_password.as_ref()))
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
            .field("password", &redacted(self.password.as_ref()))
            .finish()
    }
}

fn redacted(password: Option<&String>) -> &'static str {
    match password {
        Some(_) => "<redacted>",
        None => "None",
    }
}

/// What a run says when `sudo` wants a password and none was given, and when the one given was
/// wrong. Both texts are ansible-core 2.19.12's own, measured against `sudo` 1.9.15p5.
pub const MISSING_SUDO_PASSWORD: &str = "Missing sudo password";
pub const INCORRECT_SUDO_PASSWORD: &str = "Incorrect sudo password";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Transport {
    /// Run the agent on the controller machine itself.
    Local,
    Ssh(SshTarget),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SshTarget {
    pub address: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub private_key: Option<PathBuf>,
    pub common_args: Vec<String>,
    pub extra_args: Vec<String>,
    pub host_key_checking: bool,
    pub connect_timeout: Duration,
    /// Where the agent is cached on the host. A leading `~` is expanded by the shell that will
    /// run the agent, so a link that escalates caches it under the target user's own home and
    /// a link that does not caches it under the connecting user's.
    pub remote_tmp: String,
    /// The `ControlPath` of this inventory host's shared connection, or `None` to leave `ssh`
    /// to open one per invocation. Every `ssh` of the host goes through it: the bootstrap
    /// probe, the upload, the link and the escalated link then pay one key exchange between
    /// them instead of one each. Boxed so the two variants of a `Transport` stay close in size.
    pub control_path: Option<Box<Path>>,
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

/// Whether the bootstrap probe itself answered, which is what an escalation probe has to
/// establish before anything is written to the link. Every exit the probe script can make is
/// one of these, and nothing else on the path produces one: `sudo` refuses with its own 1, and
/// a shell that cannot find or run it says 126 or 127. So one of these codes proves the command
/// really did run as the asked-for user, whatever it then found there.
fn bootstrap_probe_ran(probe: &Captured) -> bool {
    matches!(
        probe.code,
        Some(0 | EXIT_AGENT_MISSING | EXIT_AGENT_UNRUNNABLE | EXIT_UNAME_FAILED)
    )
}

impl Transport {
    /// The connection one host uses, read from its effective variables: the merged view under
    /// the full precedence rather than the inventory object. `ansible_connection` and its
    /// neighbours are ordinary variables, so an extra var, a `group_vars` file loaded beside the
    /// playbook, a task's own `vars:` and a `set_fact` all reach here, and they are read again
    /// for every task - measured on ansible-core 2.19.12, a `set_fact` of `ansible_host` in the
    /// middle of a play is the address the next task connects to.
    pub fn for_vars(
        host_name: &str,
        vars: &Map<String, Value>,
        defaults: &ConnectionDefaults,
    ) -> anyhow::Result<Transport> {
        let setting_text = |key: &str| {
            host_setting(vars, key)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        match setting_text("ansible_connection")
            .as_deref()
            .unwrap_or("ssh")
        {
            "local" => Ok(Transport::Local),
            "ssh" => Ok(Transport::Ssh(SshTarget {
                address: setting_text("ansible_host").unwrap_or_else(|| host_name.to_string()),
                port: port_of(vars),
                user: setting_text("ansible_user").or_else(|| defaults.remote_user.clone()),
                private_key: setting_text("ansible_ssh_private_key_file")
                    .map(PathBuf::from)
                    .or_else(|| defaults.private_key.clone()),
                common_args: setting_args(host_name, vars, "ansible_ssh_common_args")?,
                extra_args: setting_args(host_name, vars, "ansible_ssh_extra_args")?,
                host_key_checking: defaults.host_key_checking,
                connect_timeout: defaults.connect_timeout,
                // Blank reads as unset, which is what the configuration arms do with the same
                // value: an empty `remote_tmp` puts the agent cache at `/volant-agent-<version>`,
                // the upload fails on permissions, and the host is reported unreachable over a
                // directory nobody wrote. Trimmed for the same reason they trim.
                remote_tmp: match setting_text("ansible_remote_tmp")
                    .map(|tmp| tmp.trim().to_string())
                    .filter(|tmp| !tmp.is_empty())
                {
                    Some(tmp) => {
                        validate_remote_tmp(&tmp)
                            .map_err(|e| anyhow::anyhow!("host '{host_name}': {e}"))?;
                        tmp
                    }
                    // The default already went through the same check when the configuration
                    // that produced it was loaded; a host variable is the one source that
                    // reaches here unchecked.
                    None => defaults.remote_tmp.clone(),
                },
                control_path: None,
            })),
            other => bail!("host '{host_name}': connection '{other}' is not supported"),
        }
    }

    /// The same, from the inventory object's own variables alone. Nothing on the run's own path
    /// calls it: the executor passes the host's effective map and the pre-flight in `cli.rs`
    /// builds its own, both through `for_vars`. What is left is the shorthand the transport's
    /// own tests are written against, and that is the point of keeping it - those tests read an
    /// inventory `Host` and were not touched when the resolution moved onto the effective view,
    /// so they are the evidence that the move changed no rule.
    pub fn for_host(host: &Host, defaults: &ConnectionDefaults) -> anyhow::Result<Transport> {
        Self::for_vars(&host.name, &as_map(&host.vars), defaults)
    }

    /// This transport with its `ssh` runs sharing one connection per inventory host, the
    /// socket under `dir` (from [`control_dir`]; `None` shares nothing). `host` is the host
    /// whose driver holds the link, and `delegate` the host it reaches for a delegated task:
    /// a delegated link is kept by its driver for the rest of the play, so twenty hosts
    /// delegating to one would put twenty sessions on one master, past sshd's `MaxSessions`
    /// of 10, if they shared the delegate's own. Options the operator wrote win: a second
    /// `ControlPath` would only fight theirs.
    pub fn shared(self, dir: Option<&Path>, host: &str, delegate: Option<&str>) -> Transport {
        match self {
            Transport::Ssh(mut target) => {
                target.control_path = None;
                if !user_sets_control(&target.common_args, &target.extra_args) {
                    target.control_path =
                        dir.and_then(|dir| control_path_for(dir, host, delegate, &target));
                }
                Transport::Ssh(target)
            }
            local => local,
        }
    }

    /// Whether this transport's `ssh` runs share a connection, through [`Transport::shared`].
    pub fn is_shared(&self) -> bool {
        matches!(self, Transport::Ssh(target) if target.control_path.is_some())
    }

    /// Makes the master of this host's shared connection exit, if there is one, so the next
    /// `ssh` opens a fresh connection instead of riding one the host may have dropped without a
    /// word. Used before reconnecting to a host that went away.
    pub async fn stop_shared(&self) {
        if let Transport::Ssh(target) = self {
            target.stop_master().await;
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
                let escalated = match escalation {
                    Some(e) => Some((e, check_local_escalation(&agent, e).await?)),
                    None => None,
                };
                let argv = local_argv(&agent, escalated);
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
                AgentLink::new_with_preamble(child, preamble(escalated))
                    .await
                    .map_err(|e| ConnectError::Unreachable(format!("{e:#}")))
            }
            Transport::Ssh(target) => target.connect(agents, escalation).await,
        }
    }
}

/// Which `sudo` form a link uses. The escalation probe settles it and the link itself then
/// uses the same one, so the two can never disagree about whether a password is offered.
///
/// `-n` is the form for a `sudo` that will not ask for anything: it never reads stdin, so
/// nothing may be written to a link opened this way. That covers a `NOPASSWD` rule as much as
/// an authentication still cached, and neither can be told from the other before asking.
///
/// `-k -S -p ''` is the form for a `sudo` that does ask, and `-k` there is not optional.
/// Measured on `sudo` 1.9.15p5 and `sudo-rs` 0.2.13: while a previous authentication is still
/// cached, `-S` does not read stdin at all, and the password written there would be read by the
/// agent as the first bytes of its first frame. `-k` drops that cached authentication, so `-S`
/// always consumes exactly the one line written for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SudoForm {
    NoPrompt,
    ReadStdin,
}

impl SudoForm {
    fn flags(self) -> &'static [&'static str] {
        match self {
            SudoForm::NoPrompt => &["-n"],
            SudoForm::ReadStdin => &["-k", "-S", "-p", ""],
        }
    }
}

/// One escalation and the `sudo` form settled for it, or `None` for a link that does not
/// escalate at all.
type Escalated<'a> = Option<(&'a Escalation, SudoForm)>;

/// The `sudo` password, as the bytes written to the child before any frame. Built here and
/// nowhere else, so the only thing that ever holds it is a `Vec<u8>` on its way to a pipe.
///
/// Nothing is written unless the probe settled on the form that reads stdin. A `sudo` that
/// needs no authentication leaves the line on the pipe, where the agent reads it as its first
/// frame header and then never answers.
fn preamble(escalated: Escalated<'_>) -> Option<Vec<u8>> {
    let (escalation, form) = escalated?;
    if form != SudoForm::ReadStdin {
        return None;
    }
    let password = escalation.password.as_ref()?;
    let mut bytes = Vec::with_capacity(password.len() + 1);
    bytes.extend_from_slice(password.as_bytes());
    bytes.push(b'\n');
    Some(bytes)
}

/// The command line that starts a local agent, under `sudo` when escalation is asked for.
fn local_argv(agent: &str, escalated: Escalated<'_>) -> Vec<String> {
    let Some((escalation, form)) = escalated else {
        return vec![agent.to_string()];
    };
    let mut argv = vec!["sudo".to_string(), "-H".to_string()];
    argv.extend(form.flags().iter().map(ToString::to_string));
    argv.extend([
        "-u".to_string(),
        escalation.user.clone(),
        "--".to_string(),
        agent.to_string(),
    ]);
    argv
}

/// One remote command as the escalated user, or unchanged when the link does not escalate.
///
/// The command travels as a single word to an inner `sh -c` rather than straight to `sudo`,
/// because `remote_tmp` starts with `~`: the outer remote shell would expand it to the
/// *connecting* user's home, which is the one directory an unprivileged `become_user` may not
/// be able to enter. Inside the shell `sudo -H` starts, `~` is the escalated user's own home,
/// so the agent is cached where that user can read it and nowhere else.
fn escalated_command(escalated: Escalated<'_>, command: &str) -> String {
    match escalated {
        None => command.to_string(),
        Some(_) => format!("{}sh -c {}", sudo_prefix(escalated), single_quoted(command)),
    }
}

/// `sudo -H ... --`, up to but not including the agent, as shell words for a remote command.
/// Empty without escalation, so one `format!` covers both cases at every call site.
fn sudo_prefix(escalated: Escalated<'_>) -> String {
    let Some((escalation, form)) = escalated else {
        return String::new();
    };
    let flags = form
        .flags()
        .iter()
        .map(|f| single_quoted(f))
        .collect::<Vec<_>>()
        .join(" ");
    format!("sudo -H {flags} -u {} -- ", single_quoted(&escalation.user))
}

/// Which `sudo` form works for this link, asked of `sudo` itself rather than guessed from
/// whether a password happens to be available.
///
/// `-n` goes first, because a `sudo` that needs no authentication never reads stdin and a
/// password written to such a link would be read by the agent as its first frame header. Only
/// a refusal for want of authentication opens the password path, and only when there is a
/// password to offer: with none, `-n`'s refusal *is* the answer, and it is already worded for
/// it. Any other refusal is final, since no password can fix a missing `sudo`, a rule that
/// forbids the command, or a `sudo` that exited zero having run something else.
/// Returns the form and what the probe that settled it printed, so a caller whose probe is a
/// command it was going to run anyway does not have to run it twice.
async fn settle_form<F, Fut>(
    escalation: &Escalation,
    ran: fn(&Captured) -> bool,
    mut probe: F,
) -> Result<(SudoForm, Captured), ConnectError>
where
    F: FnMut(SudoForm) -> Fut,
    Fut: Future<Output = Result<Captured, ConnectError>>,
{
    let quiet = probe(SudoForm::NoPrompt).await?;
    let refusal = match escalation_outcome(ran(&quiet), &quiet.stderr, None) {
        Ok(()) => return Ok((SudoForm::NoPrompt, quiet)),
        Err(refusal) => refusal,
    };
    if escalation.password.is_none() || !refused_authentication(&quiet.stderr) {
        return Err(refusal);
    }
    let offered = probe(SudoForm::ReadStdin).await?;
    escalation_outcome(
        ran(&offered),
        &offered.stderr,
        escalation.password.as_deref(),
    )
    .map(|()| (SudoForm::ReadStdin, offered))
}

/// Asks `sudo` to print the agent's version as the escalated user, before the link itself is
/// opened. A refusal is diagnosed here, from `sudo`'s own words, rather than turning up later
/// as an agent that never answered; and because the agent has not started yet, nothing has to
/// read a pipe the running agent also writes to.
async fn check_local_escalation(
    agent: &str,
    escalation: &Escalation,
) -> Result<SudoForm, ConnectError> {
    settle_form(escalation, agent_version_ran, move |form| {
        local_escalation_probe(agent, escalation, form)
    })
    .await
    .map(|(form, _)| form)
}

/// Whether `sudo` really did start the local agent as the asked-for user: the agent's own
/// version line, not `sudo`'s exit status alone. A `sudo` that exited zero having run something
/// else - a wrapper, an alias, a stale cached binary - is not an escalation that worked.
fn agent_version_ran(probe: &Captured) -> bool {
    probe.code == Some(0)
        && probe.stdout.trim() == format!("volant-agent {}", env!("CARGO_PKG_VERSION"))
}

/// One `sudo ... volant-agent --version` in the given form, and what it printed.
async fn local_escalation_probe(
    agent: &str,
    escalation: &Escalation,
    form: SudoForm,
) -> Result<Captured, ConnectError> {
    let mut argv = local_argv(agent, Some((escalation, form)));
    argv.push("--version".to_string());
    let stdin = preamble(Some((escalation, form)));
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        // The messages matched below are `sudo`'s English ones.
        .env("LC_ALL", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command
        .spawn()
        .map_err(|e| ConnectError::Become(format!("starting sudo: {e}")))?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A `sudo` that refuses before reading closes the pipe; its own words below say what
        // happened, so a broken pipe here is not the error worth reporting.
        let _ = pipe.write_all(&bytes).await;
        drop(pipe);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| ConnectError::Become(format!("waiting for sudo: {e}")))?;
    Ok(Captured {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Why `sudo` did not escalate, given that it demonstrably did not run the command it was
/// handed. `ran` is that proof, and each transport owes its own: the local one asks the agent
/// for its version, and the remote one reads one of the bootstrap's private exit codes, which
/// nothing but the bootstrap itself can produce. Either way a `sudo` that exited zero having
/// run something else is not an escalation that worked.
///
/// The strings are the two `sudo` implementations' own, measured on the development machine
/// (`sudo-rs` 0.2.13) and on the target machine (`sudo` 1.9.15p5); a host may run either, so
/// both wordings are matched. Which of the two messages a match produces is decided by whether
/// `offered` carries the password this invocation wrote, rather than by the text, whose split
/// between "none given" and "wrong one" differs between the two implementations.
fn escalation_outcome(ran: bool, stderr: &str, offered: Option<&str>) -> Result<(), ConnectError> {
    if ran {
        return Ok(());
    }
    if refused_authentication(stderr) {
        return Err(ConnectError::Become(
            if offered.is_some() {
                INCORRECT_SUDO_PASSWORD
            } else {
                MISSING_SUDO_PASSWORD
            }
            .to_string(),
        ));
    }
    Err(ConnectError::Become(first_words(
        &redacted_stderr(stderr, offered),
        "sudo refused without a message",
    )))
}

/// Whether `sudo` refused because it wanted an authentication it did not get.
fn refused_authentication(stderr: &str) -> bool {
    const AUTHENTICATION: &[&str] = &[
        "a password is required",
        "interactive authentication is required",
        "no password was provided",
        "Sorry, try again",
        "incorrect password",
        "Authentication failed",
        "Authentication required but not attempted",
    ];
    AUTHENTICATION.iter().any(|m| stderr.contains(m))
}

/// `sudo`'s own words on their way into a message an operator reads, with the password this
/// invocation offered it taken back out.
///
/// No `sudo` measured for this echoes the password it was given, and this text only ever
/// reaches a task result when `sudo` said something the two known implementations do not say.
/// The password is the controller's own, so the path is closed by construction rather than by
/// trusting every `sudo` on every host to keep it out of its stderr.
fn redacted_stderr(stderr: &str, offered: Option<&str>) -> String {
    match offered {
        Some(password) if !password.is_empty() && stderr.contains(password) => {
            stderr.replace(password, "<redacted>")
        }
        _ => stderr.to_string(),
    }
}

/// `run_capturing`'s message for an `ssh` that exited 255, with the same redaction
/// `escalation_outcome` gives `sudo`'s own refusal. `ssh`'s own words are worth keeping here -
/// they are how an operator tells a dead host from a refused key from a closed port - so this
/// scrubs the password out rather than dropping `stderr` outright.
fn unreachable_message(stderr: &str, offered: Option<&str>) -> String {
    // The `Task failed: ` prefix is the reference's own, measured: it opens the `msg` of an
    // UNREACHABLE and of a task that dies on a conditional, so a playbook testing
    // `'Task failed' in result.msg` has to find it here too.
    format!(
        "Task failed: Failed to connect to the host via ssh: {}",
        first_words(
            &redacted_stderr(stderr, offered),
            "ssh failed without a message"
        )
    )
}

/// The words of one `ansible_ssh_*_args` variable. An unbalanced quote is refused by name
/// rather than dropped: silently connecting without a `ProxyJump` or `ProxyCommand` the
/// inventory asked for can reach a different machine than the operator meant.
fn setting_args(
    host_name: &str,
    vars: &Map<String, Value>,
    key: &str,
) -> anyhow::Result<Vec<String>> {
    let Some(text) = host_setting(vars, key).and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    shlex::split(text).ok_or_else(|| {
        anyhow::anyhow!(
            "host '{host_name}': {key} has unbalanced quotes and cannot be turned into ssh options: {text}"
        )
    })
}

/// An inventory object's variables in the shape the effective view has. The two sides of the
/// engine keep different maps - the inventory is ordered by name, a host's merged view is a
/// JSON object - and only the paths with no effective view in hand pay this copy.
fn as_map(vars: &std::collections::BTreeMap<String, Value>) -> Map<String, Value> {
    vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// One remote path as a single shell word. A leading `~` or `~user` stays bare, up to and
/// including the first `/`, so the remote shell, the only thing that can, expands it; the rest
/// is single-quoted, so a `remote_tmp` holding a space or a shell metacharacter cannot split
/// into several words or run anything. That bare segment cannot be quoted without stopping the
/// shell from expanding it, so this depends on [`validate_remote_tmp`] having already refused
/// everything between the `~` and the first `/` that is not a user name.
///
/// That validator runs at every source `remote_tmp` has: `Config::load` for `[defaults]` and for
/// `ANSIBLE_REMOTE_TMP`, and [`Transport::for_vars`] for a host's own `ansible_remote_tmp`, which
/// is where a `group_vars`, a task's `vars:`, a `set_fact` or a `-e` arrives. A fourth source
/// added without a fourth call is how this guarantee rots, so the list is here rather than left
/// to a grep.
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

/// The characters a user name may hold in the `~user` part of a remote path. Everything else --
/// a space, a quote, a `$`, a backtick -- would reach the remote shell unquoted, because that
/// segment is the one thing `shell_word` cannot quote: quoting it would stop the shell from
/// expanding it, which is the only reason it is there.
fn is_home_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Refuses a `remote_tmp` whose `~user` part -- up to the first `/`, or the whole value if there
/// is none -- holds anything but a user name. A value with no leading `~` at all is untouched:
/// `shell_word` single-quotes it whole, so nothing there reaches the shell unquoted.
pub(crate) fn validate_remote_tmp(value: &str) -> anyhow::Result<()> {
    let Some(rest) = value.strip_prefix('~') else {
        return Ok(());
    };
    let user = rest.split('/').next().unwrap_or("");
    if user.chars().all(is_home_char) {
        return Ok(());
    }
    bail!(
        "remote_tmp '{value}': only a user name may follow '~', because that part reaches the \
         remote shell unquoted"
    )
}

fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `ansible_port`, whether the inventory typed it as a number or quoted it as a string.
fn port_of(vars: &Map<String, Value>) -> Option<u16> {
    let value = host_setting(vars, "ansible_port")?;
    match value {
        Value::Number(_) => value.as_u64().and_then(|p| u16::try_from(p).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// How long a shared connection outlives its last `ssh`. Long enough to carry a host from one
/// task to the next and across a barrier; the reference keeps its own for 60 seconds.
const CONTROL_PERSIST: &str = "30s";

/// The longest `ControlPath` every platform can bind. `ssh` binds the master's socket under a
/// temporary name first, the path plus `.` and 16 random characters, and renames it after, so
/// the path gets 17 bytes less than `sun_path`, which is 104 bytes with its NUL on macOS and
/// 108 on Linux. Measured with OpenSSH 10.2 on Linux: a 90-byte path binds, a 91-byte one
/// fails with `unix_listener: path "<path>.<16 characters>" too long for Unix domain socket`
/// and exit 255, which would report the host unreachable.
const CONTROL_PATH_MAX: usize = 104 - 1 - 17;

/// `ServerAliveInterval` and `ServerAliveCountMax` of a shared connection: a master whose host
/// stopped answering exits after 3 probes 5 seconds apart.
const SERVER_ALIVE_INTERVAL: u32 = 5;
const SERVER_ALIVE_COUNT_MAX: u32 = 3;

/// The client every connection runs.
const SSH_PROGRAM: &str = "ssh";

/// The `ssh -G` keywords that only time a connection and say nothing of where it goes. A
/// socket name must not change with them: `reboot`'s `connect_timeout` changes
/// `connecttimeout` for its reconnection only.
const TIMING_ONLY: &[&str] = &[
    "connecttimeout",
    "connectionattempts",
    "serveraliveinterval",
    "serveralivecountmax",
];

/// How long `ssh -O exit` may take. It only talks to a local socket, so this is never reached
/// unless the master itself is wedged.
const STOP_MASTER_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the operator's own `ssh` arguments rule connection sharing out: they already say
/// something about it, by option name (in any case, as `ssh` reads them) or by the `-S` and
/// `-M` flags that set the same two options, or they ask for debug output with `-v`.
///
/// Under `-v` a backgrounded master keeps the standard error of the `ssh` that started it
/// (OpenSSH's `control_persist_detach` leaves it open when debugging to stderr), so the first
/// bootstrap `ssh` of the host would only return once the master exits, `ControlPersist` after
/// its last client, and the link's master would hold Volant's own standard error past its exit.
fn user_sets_control(common: &[String], extra: &[String]) -> bool {
    common.iter().chain(extra).any(|word| {
        let lower = word.to_ascii_lowercase();
        ["controlmaster", "controlpath", "controlpersist"]
            .iter()
            .any(|name| lower.contains(name))
            || word.starts_with("-S")
            || word == "-M"
            || asks_for_debug(word)
    })
}

/// Whether one word is a cluster of `ssh` flags holding `-v`. The letter of an option that
/// takes a value ends the cluster, since the rest of the word is that value: `-lvolant` is a
/// user name, not a `-v`.
fn asks_for_debug(word: &str) -> bool {
    let Some(flags) = word.strip_prefix('-').filter(|f| !f.starts_with('-')) else {
        return false;
    };
    for c in flags.chars() {
        if c == 'v' {
            return true;
        }
        if "BbcDEeFIiJLlmOoPpQRSWw".contains(c) {
            return false;
        }
    }
    false
}

/// The socket of one inventory host's shared connection: `dir` and the first 16 hex characters
/// of a blake3 over the inventory name, the delegate if the link is a delegated one, and every
/// `ssh` option the host connects with. The inventory name is in it because `ssh`'s own `%C`
/// is not enough: fifty inventory aliases of one machine would share one master and run past
/// the server's `MaxSessions`. The options are in it so a connection opened with one key, user
/// or proxy never serves a host asking for another; what `ssh` itself resolves from its
/// configuration is added when the link opens (see [`SshTarget::resolved`]). `None` when the
/// path would be too long to bind.
fn control_path_for(
    dir: &Path,
    host: &str,
    delegate: Option<&str>,
    target: &SshTarget,
) -> Option<Box<Path>> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(host.as_bytes());
    if let Some(delegate) = delegate {
        hasher.update(b"\0delegate\0");
        hasher.update(delegate.as_bytes());
    }
    for word in target.ssh_argv("") {
        hasher.update(b"\0");
        hasher.update(word.as_bytes());
    }
    let path = dir.join(&hasher.finalize().to_hex()[..16]);
    (path.as_os_str().len() <= CONTROL_PATH_MAX).then(|| path.into_boxed_path())
}

/// The directory for the shared connections' sockets, for this user (see `control_dir_in`),
/// or `None` after a warning saying why there is none.
#[cfg(unix)]
pub fn control_dir() -> Option<PathBuf> {
    // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    control_dir_in(runtime.as_deref(), Path::new("/tmp"), uid)
        .map_err(|why| eprintln!("[WARNING]: ssh connections are not shared: {why}"))
        .ok()
}

/// OpenSSH for Windows has no connection sharing.
#[cfg(not(unix))]
pub fn control_dir() -> Option<PathBuf> {
    None
}

/// `<runtime>/volant-cm` when `runtime` is a directory `uid` owns and nobody else can write to,
/// `<tmp>/volant-cm-<uid>` otherwise, created at mode 0700. Anybody who can create a socket in
/// it can hand the next `ssh` a connection of their own, so an existing directory is used only
/// if it is a real directory, owned by `uid`, at mode 0700; anything else is refused with the
/// reason. Its parent must not let anybody else rename it away and plant their own: not
/// writable by group or others, unless it is sticky as `/tmp` is.
///
/// A runtime directory whose path holds anything but letters, digits and `/._-` is passed
/// over too: `ssh` reads `-o ControlPath=...` as a configuration line, where a space ends the
/// value and `%` starts a token.
#[cfg(unix)]
fn control_dir_in(runtime: Option<&Path>, tmp: &Path, uid: u32) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let plain = |p: &Path| {
        p.to_str().is_some_and(|text| {
            text.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
        })
    };
    let owned = |p: &Path| {
        std::fs::symlink_metadata(p)
            .is_ok_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o022 == 0)
    };
    let dir = if let Some(runtime) = runtime.filter(|r| plain(r) && owned(r)) {
        runtime.join("volant-cm")
    } else {
        let parent = std::fs::metadata(tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        if parent.mode() & 0o022 != 0 && parent.mode() & 0o1000 == 0 {
            return Err(format!(
                "{} is writable by others and not sticky",
                tmp.display()
            ));
        }
        tmp.join(format!("volant-cm-{uid}"))
    };
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o777 != 0o700 {
        return Err(format!(
            "{} is not a directory of mode 0700 owned by uid {uid}",
            dir.display()
        ));
    }
    Ok(dir)
}

/// Raises this process's soft limit on open files to its hard limit, capped at 2^20. Every
/// link costs the controller three descriptors, and a soft limit of 1024 left hosts
/// unreachable with `Too many open files` on a run of a few hundred. Nothing is lowered, and
/// a limit the system refuses to raise stays where it was.
#[cfg(unix)]
pub fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes into the struct it is given, which lives for the whole call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    let wanted = limit.rlim_max.min(1 << 20);
    if limit.rlim_cur < wanted {
        limit.rlim_cur = wanted;
        // SAFETY: as above; the struct is only read. A refusal leaves the limit unchanged.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    }
}

#[cfg(not(unix))]
pub fn raise_open_file_limit() {}

/// Whether an `ssh -G` dump shows connection sharing already set up. Measured with OpenSSH
/// 10.2: nothing configured prints `controlmaster false` and `controlpersist no` and no
/// `controlpath` line at all.
fn config_sets_control(dump: &str) -> bool {
    dump.lines().any(|line| match line.split_once(' ') {
        Some(("controlpath", _)) => true,
        Some(("controlmaster", value)) => value != "false",
        Some(("controlpersist", value)) => value != "no",
        _ => false,
    })
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
        let mut argv = vec![SSH_PROGRAM.to_string()];
        if compress {
            argv.push("-C".into());
        }
        argv.extend([
            "-o".to_string(),
            "BatchMode=yes".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout.as_secs()),
        ]);
        if let Some(path) = &self.control_path {
            argv.extend([
                "-o".into(),
                "ControlMaster=auto".into(),
                "-o".into(),
                format!("ControlPath={}", path.display()),
                "-o".into(),
                format!("ControlPersist={CONTROL_PERSIST}"),
            ]);
            // A master left on a connection its host dropped without a word exits on its own
            // after three unanswered probes, about 15 seconds, instead of holding every new
            // session until TCP gives up. The operator's own keepalive, if they set one, wins.
            if !self
                .common_args
                .iter()
                .chain(&self.extra_args)
                .any(|word| word.to_ascii_lowercase().contains("serveralive"))
            {
                argv.extend([
                    "-o".into(),
                    format!("ServerAliveInterval={SERVER_ALIVE_INTERVAL}"),
                    "-o".into(),
                    format!("ServerAliveCountMax={SERVER_ALIVE_COUNT_MAX}"),
                ]);
            }
        }
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

    /// Prints the machine architecture, then the cached agent's version and the hash of its own
    /// file, or exits 42 when no agent is cached. The architecture names the binary this
    /// controller would upload, whose hash decides whether the cached one is reused. A cached
    /// agent that cannot run exits 45 and `uname` failing exits 46, so neither hands its own
    /// status back to `ssh`, where 255 would read as a connection failure.
    pub fn probe_command(&self) -> String {
        format!(
            "uname -m || exit {EXIT_UNAME_FAILED}; a={agent}; if [ -x \"$a\" ]; then \"$a\" --version --build-id || {{ echo \"exit status $?\" >&2; exit {EXIT_AGENT_UNRUNNABLE}; }}; else exit {EXIT_AGENT_MISSING}; fi",
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

    /// Opens a link, through the shared connection unless the operator's own ssh configuration
    /// already multiplexes this host, in which case theirs is used and this one stays out of it.
    async fn connect(
        &self,
        agents: &AgentSource,
        escalation: Option<&Escalation>,
    ) -> Result<AgentLink, ConnectError> {
        self.resolved().await.open(agents, escalation).await
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
        let mut argv = bare.ssh_argv(":");
        argv.insert(1, "-G".into());
        let dump = match Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
        {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
            _ => return bare,
        };
        if config_sets_control(&dump) {
            return bare;
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
            ..bare
        }
    }

    /// `ssh -O exit` on this host's shared connection, if it has one: the master exits and the
    /// next `ssh` opens a new connection. A master whose host went away without closing the
    /// connection would otherwise take every new session and leave it waiting on TCP
    /// retransmissions, which `ConnectTimeout` does not bound. The status is not read: no
    /// master is the state this asks for. The whole of it, the `ssh -G` that finds the socket
    /// included, is bounded by [`STOP_MASTER_TIMEOUT`].
    async fn stop_master(&self) {
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
        let (escalated, probed) = match escalation {
            Some(e) => {
                let (form, probe) = self.check_escalation(e).await?;
                (Some((e, form)), Some(probe))
            }
            None => (None, None),
        };
        self.bootstrap(agents, escalated, probed)
            .await
            .map_err(|err| match (err, escalated) {
                (ConnectError::Unreachable(msg), Some((e, _))) => ConnectError::Unreachable(
                    format!("preparing the agent for user {}: {msg}", e.user),
                ),
                (err, _) => err,
            })?;
        let agent = shell_word(&self.agent_path());
        let remote = match escalated {
            None => format!("exec {agent}"),
            Some(_) => format!(
                "exec {}",
                escalated_command(escalated, &format!("exec {agent}"))
            ),
        };
        let child = Command::new(SSH_PROGRAM)
            .args(&self.ssh_argv(&remote)[1..])
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
            self.run_bootstrap(Some((escalation, form)), &command, None)
        })
        .await
    }

    /// One bootstrap command, run as the escalated user where the link escalates.
    ///
    /// The `sudo` password goes on stdin ahead of any payload, never after it: with the
    /// `ReadStdin` form `sudo` reads exactly one line before the command it starts sees
    /// anything, so an upload whose first line went to `sudo` would land one line short.
    async fn run_bootstrap(
        &self,
        escalated: Escalated<'_>,
        command: &str,
        payload: Option<&[u8]>,
    ) -> Result<Captured, ConnectError> {
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
    async fn bootstrap(
        &self,
        agents: &AgentSource,
        escalated: Escalated<'_>,
        probed: Option<Captured>,
    ) -> Result<(), ConnectError> {
        let probe = match probed {
            Some(probe) => probe,
            None => {
                self.run_bootstrap(escalated, &self.probe_command(), None)
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
                        escalated,
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
            .run_bootstrap(escalated, &self.upload_command(size), Some(&file.bytes))
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
            .run_bootstrap(escalated, &self.probe_command(), None)
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
            return Err(ConnectError::Unreachable(unreachable_message(
                &captured.stderr,
                offered,
            )));
        }
        Ok(captured)
    }
}

#[derive(Debug)]
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

/// Whether the probe ran exactly the binary hashed as `hash` from the cache: the agent it found
/// printed this version and a hash of its own file equal to the blake3 hash of the binary this
/// controller would upload. The version alone proves nothing, since any build of this source
/// reports it.
fn is_this_agent(probe: &Captured, hash: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn host(vars: Value) -> Host {
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

    /// `-n` refuses to ask, so a host that wants a password fails fast instead of blocking on a
    /// prompt nothing will ever answer. In the form that does read the password, `-k` is what
    /// makes `-S` read stdin at all: measured on `sudo` 1.9.15p5 and `sudo-rs` 0.2.13, a
    /// still-valid authentication makes `-S` leave the password on the pipe, where the agent
    /// would read it as the first bytes of its first frame.
    #[test]
    fn the_sudo_command_line_names_the_user_and_asks_for_the_password_on_stdin() {
        let password = format!("only-this-run-{}", std::process::id());
        let secret = escalation(Some(password.as_str()));
        let argv = local_argv("/usr/bin/volant-agent", Some((&secret, SudoForm::NoPrompt)));
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
        let argv = local_argv(
            "/usr/bin/volant-agent",
            Some((&secret, SudoForm::ReadStdin)),
        );
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
            !argv.contains(&password),
            "the password never reaches a command line: {argv:?}"
        );
        assert_eq!(
            local_argv("/usr/bin/volant-agent", None),
            ["/usr/bin/volant-agent"],
            "without escalation the agent is the command"
        );
    }

    /// The preamble follows the form the probe settled on, not whether a password happens to be
    /// available: a `sudo` that needs no authentication never reads stdin, and the line written
    /// for it would be read by the agent as its first frame header.
    #[test]
    fn only_the_form_that_reads_stdin_is_given_the_password() {
        let password = format!("only-this-run-{}", std::process::id());
        let secret = escalation(Some(password.as_str()));
        assert_eq!(
            preamble(Some((&secret, SudoForm::ReadStdin))),
            Some(format!("{password}\n").into_bytes())
        );
        assert_eq!(preamble(Some((&secret, SudoForm::NoPrompt))), None);
        assert_eq!(
            preamble(Some((&escalation(None), SudoForm::ReadStdin))),
            None
        );
        assert_eq!(preamble(None), None);
    }

    #[test]
    fn the_remote_sudo_prefix_quotes_the_user_and_carries_no_password() {
        let password = format!("only-this-run-{}", std::process::id());
        let secret = escalation(Some(password.as_str()));
        let prefix = sudo_prefix(Some((&secret, SudoForm::ReadStdin)));
        assert_eq!(prefix, "sudo -H '-k' '-S' '-p' '' -u 'deploy' -- ");
        assert!(!prefix.contains(&password), "{prefix}");
        assert_eq!(
            sudo_prefix(Some((
                &Escalation {
                    user: "it's me".into(),
                    password: None
                },
                SudoForm::NoPrompt
            ))),
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
        let outcome = |stderr: &str, offered| match escalation_outcome(false, stderr, offered) {
            Err(ConnectError::Become(msg)) => msg,
            other => panic!("expected Become, got {other:?}"),
        };
        let password = format!("only-this-run-{}", std::process::id());
        for stderr in [
            "sudo: a password is required",
            "sudo: interactive authentication is required",
        ] {
            assert_eq!(outcome(stderr, None), MISSING_SUDO_PASSWORD, "{stderr}");
        }
        for stderr in [
            "Sorry, try again.\n\nsudo: no password was provided",
            "sudo: Authentication failed, try again.",
            "sudo: Authentication required but not attempted",
            "sudo: 1 incorrect password attempt",
        ] {
            assert_eq!(
                outcome(stderr, Some(password.as_str())),
                INCORRECT_SUDO_PASSWORD,
                "{stderr}"
            );
        }
        assert_eq!(
            outcome("sh: 1: sudo: not found", None),
            "sh: 1: sudo: not found",
            "a host without sudo gets the shell's own words, not a guess about passwords"
        );
        assert_eq!(
            outcome("", None),
            "sudo refused without a message",
            "a silent refusal still says something"
        );
    }

    /// `sudo`'s own words are the message when it says something neither known implementation
    /// says, and that message reaches a task result. No measured `sudo` echoes the password it
    /// was offered, but the password belongs to the controller, so the path is closed by
    /// construction rather than by trusting every `sudo` on every host.
    #[test]
    fn sudos_own_words_cannot_carry_the_password_back_into_a_message() {
        let password = format!("only-this-run-{}", std::process::id());
        let stderr = format!("sudo: bespoke build read '{password}' and disliked it");
        let Err(ConnectError::Become(msg)) =
            escalation_outcome(false, &stderr, Some(password.as_str()))
        else {
            panic!("expected Become");
        };
        // Neither assertion prints `msg`. A failure here is a run where the redaction did not
        // happen, so the failure output would be one more place carrying the password.
        assert!(!msg.contains(&password));
        assert!(
            msg.contains("<redacted>") && msg.contains("disliked it"),
            "the rest of what sudo said survives"
        );
    }

    /// `run_capturing`'s 255 arm is fed by the same `ReadStdin` preamble that writes the
    /// password to a child's stdin for the escalation probe, so it owes the password the same
    /// redaction `sudo`'s own refusal gets. No `ssh` measured here echoes its stdin into its own
    /// stderr, but this is the only other path a password reaches a captured stderr from, and a
    /// scrubber with one way around it is worth less than it looks.
    #[test]
    fn a_dying_sshs_own_words_are_redacted_the_same_way_sudos_refusal_is() {
        let password = format!("only-this-run-{}", std::process::id());
        let stderr = format!("client_loop: send disconnect: Broken pipe reading '{password}'");
        let msg = unreachable_message(&stderr, Some(password.as_str()));
        // Neither assertion prints `msg`: a failure here is a run where the redaction did not
        // happen, so the failure output would be one more place carrying the password.
        assert!(!msg.contains(&password));
        assert!(msg.contains("<redacted>") && msg.contains("Broken pipe"));
    }

    fn agent_version() -> Captured {
        Captured {
            code: Some(0),
            stdout: format!("volant-agent {}", env!("CARGO_PKG_VERSION")),
            stderr: String::new(),
        }
    }

    fn refused(stderr: &str) -> Captured {
        Captured {
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    /// The `sudo` form is settled by asking `sudo`, not by whether a password happens to be
    /// available. A `sudo` that answers `-n` needs no authentication - a `NOPASSWD` rule, or an
    /// authentication still cached - and never reads stdin, so nothing may be written to a link
    /// opened that way even when the operator did supply a password: the line would sit on the
    /// pipe for the agent to read as its first frame header. Only a refusal for want of
    /// authentication opens the password path, and with no password to offer that refusal is
    /// itself the answer.
    #[tokio::test]
    async fn the_probe_settles_the_form_by_asking_sudo_before_offering_anything() {
        use std::cell::RefCell;
        use std::future::ready;

        let password = format!("only-this-run-{}", std::process::id());
        let with = escalation(Some(password.as_str()));
        let without = escalation(None);

        let asked = RefCell::new(Vec::new());
        let (form, _) = settle_form(&with, agent_version_ran, |form| {
            asked.borrow_mut().push(form);
            ready(Ok(agent_version()))
        })
        .await
        .expect("a sudo that answers -n escalates");
        assert_eq!(form, SudoForm::NoPrompt);
        assert_eq!(
            *asked.borrow(),
            [SudoForm::NoPrompt],
            "a sudo that needs no authentication is never asked a second time"
        );
        assert_eq!(
            preamble(Some((&with, form))),
            None,
            "and is never written a password it would leave on the pipe"
        );

        let asked = RefCell::new(Vec::new());
        let (form, _) = settle_form(&with, agent_version_ran, |form| {
            asked.borrow_mut().push(form);
            ready(Ok(match form {
                SudoForm::NoPrompt => refused("sudo: a password is required"),
                SudoForm::ReadStdin => agent_version(),
            }))
        })
        .await
        .expect("the password answers the refusal");
        assert_eq!(form, SudoForm::ReadStdin);
        assert_eq!(*asked.borrow(), [SudoForm::NoPrompt, SudoForm::ReadStdin]);
        assert!(preamble(Some((&with, form))).is_some());

        let asked = RefCell::new(Vec::new());
        let err = settle_form(&without, agent_version_ran, |form| {
            asked.borrow_mut().push(form);
            ready(Ok(refused("sudo: a password is required")))
        })
        .await
        .expect_err("no password to offer");
        assert_eq!(err.to_string(), MISSING_SUDO_PASSWORD);
        assert_eq!(
            *asked.borrow(),
            [SudoForm::NoPrompt],
            "with nothing to offer there is nothing to try twice"
        );

        let err = settle_form(&with, agent_version_ran, |form| {
            ready(Ok(match form {
                SudoForm::NoPrompt => refused("sudo: a password is required"),
                SudoForm::ReadStdin => refused("sudo: 1 incorrect password attempt"),
            }))
        })
        .await
        .expect_err("the password was wrong");
        assert_eq!(err.to_string(), INCORRECT_SUDO_PASSWORD);

        let asked = RefCell::new(Vec::new());
        let err = settle_form(&with, agent_version_ran, |form| {
            asked.borrow_mut().push(form);
            ready(Ok(refused("sh: 1: sudo: not found")))
        })
        .await
        .expect_err("no sudo at all");
        assert_eq!(err.to_string(), "sh: 1: sudo: not found");
        assert_eq!(
            *asked.borrow(),
            [SudoForm::NoPrompt],
            "a refusal no password can answer is final"
        );
    }

    /// A `sudo` that exits zero having run something else is not an escalation that worked.
    /// Checking the agent's own version line is what keeps a wrapper, an alias or a stale
    /// cached binary from being read as a success and then running the task as the wrong user.
    #[test]
    fn local_escalation_succeeds_only_on_the_agents_own_version_line() {
        let printed = |code, stdout: &str| {
            agent_version_ran(&Captured {
                code,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        };
        let version = format!("volant-agent {}", env!("CARGO_PKG_VERSION"));
        assert!(printed(Some(0), &version));
        assert!(
            printed(Some(0), &format!("{version}\n")),
            "a trailing newline is not a different version"
        );
        assert!(
            !printed(Some(0), "volant-agent 0.0.0-stale"),
            "another version is not this agent"
        );
        assert!(
            !printed(Some(0), ""),
            "a zero exit with nothing printed proves nothing"
        );
        assert!(!printed(Some(1), &version), "a refusal is a refusal");
    }

    /// The remote probe cannot ask for a version, because it is the upload of that very agent
    /// it settles the `sudo` form for. It reads the probe script's own exit codes instead: no
    /// `sudo` refusal and no shell complaint about a command it could not start produces one,
    /// so any of them proves the script ran as the asked-for user.
    #[test]
    fn the_remote_escalation_probe_is_proved_by_the_bootstraps_own_exit_codes() {
        let exited = |code| {
            bootstrap_probe_ran(&Captured {
                code,
                stdout: String::new(),
                stderr: String::new(),
            })
        };
        for code in [
            0,
            EXIT_AGENT_MISSING,
            EXIT_AGENT_UNRUNNABLE,
            EXIT_UNAME_FAILED,
        ] {
            assert!(exited(Some(code)), "the probe script's own code {code}");
        }
        for code in [1, 126, 127, EXIT_SSH_FAILURE] {
            assert!(
                !exited(Some(code)),
                "{code} belongs to sudo, the shell or ssh, not to the probe"
            );
        }
        assert!(!exited(None), "a command killed by a signal proves nothing");
    }

    /// A bootstrap command that escalates goes to an inner `sh -c`, because `remote_tmp` starts
    /// with `~`: expanded by the outer remote shell it would name the connecting user's home,
    /// the one directory an unprivileged `become_user` may not be allowed to enter.
    #[test]
    fn an_escalated_bootstrap_command_is_expanded_by_a_shell_running_as_that_user() {
        let secret = escalation(None);
        let plain = escalated_command(None, "d=~/'.ansible/tmp'; mkdir -p \"$d\"");
        assert_eq!(
            plain, "d=~/'.ansible/tmp'; mkdir -p \"$d\"",
            "a link that does not escalate is not wrapped at all"
        );
        let wrapped = escalated_command(
            Some((&secret, SudoForm::NoPrompt)),
            "d=~/'.ansible/tmp'; mkdir -p \"$d\"",
        );
        assert_eq!(
            wrapped,
            r#"sudo -H '-n' -u 'deploy' -- sh -c 'd=~/'\''.ansible/tmp'\''; mkdir -p "$d"'"#
        );
        // Run the wrapping against a real `sh`, with the `sudo` words dropped: the failure this
        // guards against is a quoting one, and what has to hold is that the tilde reaches the
        // inner shell unexpanded, so the escalated user's own `HOME` is what it names. The
        // outer shell stands in for the remote one and the inner for the one `sudo` starts.
        #[cfg(unix)]
        {
            let inner = format!("printf %s {}", shell_word("~/a b/c"));
            let wrapped = escalated_command(Some((&secret, SudoForm::NoPrompt)), &inner);
            let (_, sh) = wrapped
                .split_once("-- ")
                .expect("the sudo words end with --");
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(sh)
                .env("HOME", "/tmp/volant home")
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                "/tmp/volant home/a b/c",
                "the tilde expands where it should and the rest stays one word"
            );
        }
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

    /// A host variable is the one source of `remote_tmp` that used to be taken at face value.
    /// `-e ansible_remote_tmp=` reached `cache_dir()` as an empty path, which puts the agent at
    /// `/volant-agent-<version>`, so the upload fails on permissions at the filesystem root and
    /// the host is reported unreachable over a directory the operator never wrote. The file and
    /// the environment arms already read a blank value as no value; this one now does too.
    ///
    /// What would make this red: the blank reaching `SshTarget`, which the assertion on the
    /// cache path catches wherever the emptiness is finally noticed.
    #[test]
    fn a_blank_host_remote_tmp_reads_as_no_value() {
        for blank in ["", "   "] {
            let Transport::Ssh(target) =
                Transport::for_host(&host(json!({ "ansible_remote_tmp": blank })), &defaults())
                    .unwrap()
            else {
                panic!("expected ssh")
            };
            assert_eq!(target.remote_tmp, "~/.ansible/tmp", "{blank:?}");
        }
        let Transport::Ssh(target) = Transport::for_host(
            &host(json!({ "ansible_remote_tmp": "  /var/tmp/v  " })),
            &defaults(),
        )
        .unwrap() else {
            panic!("expected ssh")
        };
        assert_eq!(target.remote_tmp, "/var/tmp/v");
    }

    /// The host-variable table on the connections page is a hand-written copy of the names
    /// this module reads, and the page has already outlived one rewrite of `for_vars` without
    /// anybody opening it. The prose in the second column is not a function of anything the run
    /// reads, so it stays hand-written; the set of names is, so it is checked here.
    ///
    /// Every `ansible_*` literal in this file is a connection variable, which is why the whole
    /// source is the input rather than one function: a name read by `port_of` or `split_args`
    /// belongs on that page exactly as much as one read in `for_vars`. The four `become`
    /// variables under it come from `executor/prepare.rs` and are not in this set.
    ///
    /// What would make this red: a connection variable added, renamed or dropped here without
    /// the page following. A new `ansible_*` literal that is deliberately not read -- a test
    /// proving one is ignored, say -- is red too, and the answer is to give the page a row
    /// saying so rather than to loosen this.
    #[test]
    fn the_published_host_variables_are_the_ones_this_module_reads() {
        let source = include_str!("transport.rs");
        let mut read: Vec<&str> = source
            .split("\"ansible_")
            .skip(1)
            .filter_map(|rest| rest.split_once('"').map(|(name, _)| name))
            .filter(|name| {
                !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
            })
            .collect();
        read.sort_unstable();
        read.dedup();

        let page = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/src/content/docs/hosts/connections.md"),
        )
        .expect("the connections page is in the repository");
        let mut published: Vec<&str> = page
            .lines()
            .filter_map(|line| line.strip_prefix("| `ansible_"))
            .filter_map(|rest| rest.split_once('`').map(|(name, _)| name))
            .filter(|name| !name.starts_with("become"))
            .collect();
        published.sort_unstable();
        published.dedup();
        assert_eq!(
            published, read,
            "the connections page and Transport::for_vars disagree about the host variables"
        );
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

    /// A cached agent is reused only when it is the very binary this controller would upload.
    /// Its version string proves nothing: a build carrying extra instrumentation reports the
    /// same one, and a host whose cache held such a build had it run as the agent, silently.
    /// Each payload goes through the real upload and the real probe under `sh`, and the
    /// probe's answer through the same check `bootstrap` makes.
    ///
    /// What would make this red: the reuse check comparing the version alone, which reads the
    /// planted build, and the script printing this version, as this controller's agent.
    #[cfg(unix)]
    #[test]
    fn a_cached_agent_of_this_version_but_other_bytes_is_not_reused() {
        use std::io::Write;
        use std::process::{Command as SyncCommand, Stdio};

        let exe = std::env::current_exe().expect("this test's own binary path");
        let local = exe
            .parent()
            .and_then(Path::parent)
            .expect("deps/ has a parent")
            .join("volant-agent");
        let ours = std::fs::read(&local).expect("the agent is built beside the tests");
        let ours_hash = blake3::hash(&ours).to_hex().to_string();
        let base = std::env::temp_dir().join(format!("volant-cache-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let Transport::Ssh(target) = Transport::for_host(
            &host(json!({ "ansible_remote_tmp": base.to_str().unwrap() })),
            &defaults(),
        )
        .unwrap() else {
            panic!()
        };
        let cache_then_probe = |payload: &[u8]| {
            let mut child = SyncCommand::new("sh")
                .arg("-c")
                .arg(target.upload_command(payload.len() as u64))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(payload).unwrap();
            let upload = child.wait_with_output().unwrap();
            assert!(upload.status.success(), "{upload:?}");
            let probe = SyncCommand::new("sh")
                .arg("-c")
                .arg(target.probe_command())
                .stdin(Stdio::null())
                .output()
                .unwrap();
            Captured {
                code: probe.status.code(),
                stdout: String::from_utf8_lossy(&probe.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&probe.stderr).into_owned(),
            }
        };

        let mut instrumented = ours.clone();
        instrumented.extend_from_slice(b"one more section");
        let probe = cache_then_probe(&instrumented);
        assert_eq!(probe.code, Some(0), "the other build runs: {probe:?}");
        assert!(
            !is_this_agent(&probe, &ours_hash),
            "a build of this version with other bytes is not this agent: {probe:?}"
        );

        let script = format!(
            "#!/bin/sh\necho volant-agent {}\n",
            env!("CARGO_PKG_VERSION")
        );
        let probe = cache_then_probe(script.as_bytes());
        assert!(
            !is_this_agent(&probe, &ours_hash),
            "a script printing this version is not this agent: {probe:?}"
        );

        let probe = cache_then_probe(&ours);
        assert!(
            is_this_agent(&probe, &ours_hash),
            "the controller's own agent is reused: {probe:?}"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The other direction, and the one that says the rule is not simply "refuse every tilde":
    /// `~`, `~root` and `~some.user-1` are user names and still reach the remote shell bare,
    /// which is the only thing that can expand them.
    ///
    /// What would make this red: a validator refusing every `~`, which would break the default
    /// `remote_tmp` and every inventory that sets one.
    #[test]
    fn an_ordinary_tilde_user_is_still_passed_through() {
        for value in ["~", "~root", "~some.user-1"] {
            assert!(validate_remote_tmp(value).is_ok(), "{value}");
        }
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

    /// Two ssh identities of one name are two links. The name and the escalated user are equal
    /// here and only the port differs, so the key can only tell them apart through the whole
    /// `SshTarget` -- which is what tells a task that asked for port 2223 from one that asked
    /// for 2222.
    ///
    /// What would make this red: a hand-written `Hash` or `PartialEq` on `SshTarget` skipping a
    /// field. Nothing else in the repository holds the derive in place, and a skipped `port`,
    /// `user` or `private_key` would send the second task down the first one's link.
    #[test]
    fn two_ports_of_one_host_are_two_keys() {
        use crate::executor::LinkKey;
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let key = |port| LinkKey {
            host: "node".to_string(),
            become_user: None,
            transport: Transport::for_vars(
                "node",
                json!({"ansible_host": "10.0.0.5", "ansible_port": port})
                    .as_object()
                    .expect("an object"),
                &defaults(),
            )
            .expect("an ssh transport"),
        };
        let digest = |k: &LinkKey| {
            let mut h = DefaultHasher::new();
            k.hash(&mut h);
            h.finish()
        };

        assert_ne!(key(2222), key(2223));
        assert_ne!(digest(&key(2222)), digest(&key(2223)));
        assert_eq!(key(2222), key(2222));
    }

    const SHARED: Option<&str> = Some("/run/user/1000/volant-cm");

    fn ssh_target(name: &str, vars: Value, dir: Option<&str>) -> SshTarget {
        match Transport::for_vars(name, vars.as_object().expect("an object"), &defaults())
            .map(|t| t.shared(dir.map(Path::new), name, None))
        {
            Ok(Transport::Ssh(target)) => target,
            other => panic!("expected an ssh transport, got {other:?}"),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new("/tmp").join(format!("volant-cm-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// The three options come as one block, in the order the reference writes them, ahead of
    /// every word the operator wrote.
    #[test]
    fn a_shared_connection_puts_its_three_options_before_the_operators_words() {
        let target = ssh_target(
            "web1",
            json!({"ansible_ssh_common_args": "-o ProxyJump=bastion"}),
            SHARED,
        );
        let path = target.control_path.clone().expect("a control path");
        assert!(path.starts_with("/run/user/1000/volant-cm"), "{path:?}");
        let argv = target.ssh_argv("true");
        let block = [
            "-o".to_string(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", path.display()),
            "-o".into(),
            "ControlPersist=30s".into(),
        ];
        let at = argv
            .windows(block.len())
            .position(|w| w == block)
            .unwrap_or_else(|| panic!("the three options, in order: {argv:?}"));
        let user = argv
            .iter()
            .position(|w| w == "ProxyJump=bastion")
            .expect("the operator's option");
        assert!(at < user, "{argv:?}");
    }

    /// A shared connection carries a keepalive, so its master exits by itself once its host
    /// stops answering; an operator's own keepalive is left to win, and a connection Volant
    /// does not share gets none from Volant.
    ///
    /// What would make this red: the two options dropped, or added over the operator's own.
    #[test]
    fn a_shared_connection_keeps_itself_alive_or_dies() {
        let argv = ssh_target("web1", json!({}), SHARED).ssh_argv("true");
        let block = [
            "-o".to_string(),
            "ServerAliveInterval=5".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
        ];
        assert!(argv.windows(block.len()).any(|w| w == block), "{argv:?}");
        for (vars, why) in [
            (json!({}), "not shared"),
            (
                json!({"ansible_ssh_common_args": "-o ControlPath=/x"}),
                "the operator's own sharing",
            ),
            (
                json!({"ansible_ssh_extra_args": "-o ServerAliveInterval=60"}),
                "the operator's own keepalive",
            ),
        ] {
            let dir = if why == "not shared" { None } else { SHARED };
            let text = ssh_target("web1", vars, dir).ssh_argv("true").join(" ");
            assert!(!text.contains("ServerAliveInterval=5"), "{why}: {text}");
        }
    }

    /// Review focus: an operator who already set up connection sharing keeps theirs, whichever
    /// variable carries it and however it is spelled. A second `ControlPath` from Volant would
    /// silently win over theirs, since `ssh` keeps the first value it reads.
    ///
    /// What would make this red: `user_sets_control` reading one of the two variables only, or
    /// matching the option names in one case only.
    #[test]
    fn user_control_options_are_left_alone() {
        for (key, words) in [
            ("ansible_ssh_common_args", "-o ControlPath=/x"),
            ("ansible_ssh_extra_args", "-o ControlPath=/x"),
            ("ansible_ssh_extra_args", "-o ControlPersist=5m"),
            ("ansible_ssh_common_args", "-ocontrolmaster=no"),
            ("ansible_ssh_common_args", "-S /x"),
            ("ansible_ssh_extra_args", "-M"),
            ("ansible_ssh_extra_args", "-vvv"),
            ("ansible_ssh_common_args", "-4v"),
        ] {
            let target = ssh_target("web1", json!({ key: words }), SHARED);
            assert_eq!(target.control_path, None, "{key}={words}");
            let text = target.ssh_argv("true").join(" ");
            assert!(
                !text.contains("ControlMaster=auto") && !text.contains("volant-cm"),
                "{key}={words}: {text}"
            );
        }
        let plain = ssh_target("web1", json!({}), SHARED);
        assert!(plain.control_path.is_some(), "nothing set, so shared");
        for word in ["-lvolant", "-o", "LogLevel=DEBUG3", "-ivault.pem", "--"] {
            assert!(!asks_for_debug(word), "{word}");
        }
    }

    /// The same review focus, for the operator's ssh configuration file: `ssh -G` reads it the
    /// way the connection will. The file is passed with `-F` so the test does not depend on the
    /// account it runs under.
    ///
    /// What would make this red: `ssh -G` asked with Volant's own options on its command line,
    /// which it then reports back, so the empty configuration would read as set.
    #[tokio::test]
    async fn a_control_path_in_the_ssh_configuration_is_left_alone() {
        let dir = scratch("config");
        let file = dir.join("config");
        let target = ssh_target(
            "web1",
            json!({"ansible_ssh_common_args": format!("-F {}", file.display())}),
            SHARED,
        );
        assert!(target.control_path.is_some());
        std::fs::write(&file, "").expect("the configuration file");
        assert!(
            target.resolved().await.control_path.is_some(),
            "an empty configuration shares nothing; ssh -G must be able to run here"
        );
        for text in [
            "ControlPath /tmp/elsewhere-%C\n",
            "Host web1\n  ControlMaster auto\n",
            "ControlPersist 10m\n",
        ] {
            std::fs::write(&file, text).expect("the configuration file");
            assert_eq!(target.resolved().await.control_path, None, "{text:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What `ssh` resolves from its configuration is not on Volant's command line, so it goes
    /// into the socket name when the link opens: a `HostName` changed between two runs must not
    /// send the second run down the first one's master, still alive for `ControlPersist`.
    ///
    /// What would make this red: the resolved name hashing Volant's options alone.
    #[tokio::test]
    async fn a_host_moved_in_the_ssh_configuration_gets_a_new_socket() {
        let dir = scratch("resolved");
        let file = dir.join("config");
        let target = ssh_target(
            "web1",
            json!({"ansible_ssh_common_args": format!("-F {}", file.display())}),
            SHARED,
        );
        let mut paths = Vec::new();
        for text in [
            "Host web1\n  HostName 10.0.0.1\n",
            "Host web1\n  HostName 10.0.0.2\n",
            "Host web1\n  HostName 10.0.0.2\n  User ops\n",
            "Host web1\n  HostName 10.0.0.2\n  User ops\n  ProxyJump bastion\n",
            "Host web1\n  HostName 10.0.0.2\n  User ops\n  IdentityFile /k\n",
        ] {
            std::fs::write(&file, text).expect("the configuration file");
            let path = target.resolved().await.control_path.expect("shared");
            assert_eq!(path.parent(), Some(Path::new("/run/user/1000/volant-cm")));
            assert_eq!(
                path.as_os_str().len(),
                "/run/user/1000/volant-cm/".len() + 16
            );
            assert_eq!(
                target.resolved().await.control_path.as_deref(),
                Some(&*path),
                "one configuration, one socket: {text:?}"
            );
            paths.push(path);
        }
        let mut unique = paths.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), paths.len(), "{paths:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A delegated link is kept by the delegating host's driver until the end of the play, so
    /// the delegate's own master would carry one session per delegating host, and sshd refuses
    /// the eleventh (`MaxSessions` 10). Each (host, delegate) pair gets its own master instead.
    ///
    /// What would make this red: the key built from the delegate's name alone.
    #[test]
    fn delegating_hosts_do_not_pile_onto_one_master() {
        let vars = json!({"ansible_host": "10.0.0.9"});
        let own = ssh_target("lb1", vars.clone(), SHARED).control_path;
        let mut paths: Vec<_> = (1..=20)
            .map(|n| {
                let transport =
                    Transport::for_vars("lb1", vars.as_object().expect("an object"), &defaults())
                        .expect("an ssh transport")
                        .shared(SHARED.map(Path::new), &format!("web{n}"), Some("lb1"));
                match transport {
                    Transport::Ssh(target) => target.control_path.expect("shared"),
                    Transport::Local => panic!("expected ssh"),
                }
            })
            .collect();
        assert!(paths.iter().all(|p| Some(p) != own.as_ref()));
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), 20, "one master per delegating host");

        // Two inventory aliases of one delegate, reached from one host: two masters, as the
        // aliases get when they are the hosts themselves.
        let via = |delegate: &str| match Transport::for_vars(
            delegate,
            vars.as_object().expect("an object"),
            &defaults(),
        )
        .expect("an ssh transport")
        .shared(SHARED.map(Path::new), "web1", Some(delegate))
        {
            Transport::Ssh(target) => target.control_path,
            Transport::Local => panic!("expected ssh"),
        };
        assert_ne!(via("lb1"), via("lb1-alias"));
    }

    /// `ssh -O exit` reaches the socket the link would use, and returns even though nothing
    /// answers there the way a master would.
    ///
    /// What would make this red: the command sent to another path, or not bounded.
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_shared_connection_speaks_to_its_socket() {
        let dir = scratch("stop");
        let mut target = ssh_target(
            "web1",
            json!({"ansible_ssh_common_args": "-F /dev/null"}),
            None,
        );
        target.control_path = Some(dir.join("0123456789abcdef").into_boxed_path());
        let socket = target.resolved().await.control_path.expect("shared");
        let listener = std::os::unix::net::UnixListener::bind(&*socket).expect("a socket");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking socket");
        let transport = Transport::Ssh(target);
        let (_, accepted) = tokio::join!(
            tokio::time::timeout(STOP_MASTER_TIMEOUT * 2, transport.stop_shared()),
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    // Accepted and dropped at once: `ssh -O` then reads end of file and gives up.
                    match listener.accept() {
                        Ok(_) => return true,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(_) => return false,
                    }
                }
            }),
        );
        assert_eq!(accepted, Ok(true), "ssh -O exit never came to {socket:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Measured: fifty inventory aliases of one machine sharing one master ran past the
    /// server's `MaxSessions`, so the inventory name is part of the key.
    ///
    /// What would make this red: the key hashed without the inventory name.
    #[test]
    fn two_inventory_names_of_one_address_are_two_connections() {
        let vars = json!({"ansible_host": "10.0.0.5"});
        let one = ssh_target("web1", vars.clone(), SHARED);
        let two = ssh_target("web2", vars.clone(), SHARED);
        assert_eq!(one.address, two.address);
        assert_ne!(one.control_path, two.control_path);
        assert_eq!(
            one.control_path,
            ssh_target("web1", vars, SHARED).control_path,
            "one name, one path"
        );
        let other_user = ssh_target(
            "web1",
            json!({"ansible_host": "10.0.0.5", "ansible_user": "ops"}),
            SHARED,
        );
        assert_ne!(one.control_path, other_user.control_path);
    }

    /// `ssh` binds the socket under the path plus 17 bytes before renaming it, and macOS allows
    /// 103: a longer path fails every connection of the host. Such a path is not used.
    #[test]
    fn a_control_path_too_long_to_bind_is_not_used() {
        let path = ssh_target("web1", json!({}), SHARED)
            .control_path
            .expect("a control path");
        assert!(path.as_os_str().len() <= CONTROL_PATH_MAX, "{path:?}");
        // A 69-byte directory makes an 86-byte path, the longest that binds everywhere.
        let longest = format!("/{}", "x".repeat(68));
        let path = ssh_target("web1", json!({}), Some(&longest)).control_path;
        assert_eq!(path.map(|p| p.as_os_str().len()), Some(CONTROL_PATH_MAX));
        let long = format!("/{}", "x".repeat(69));
        let target = ssh_target("web1", json!({}), Some(&long));
        assert_eq!(target.control_path, None);
        assert!(!target.ssh_argv("true").join(" ").contains("Control"));
    }

    /// Anybody able to put a socket in the directory can hand the next `ssh` a connection of
    /// their own, so an existing directory must be this user's, at mode 0700.
    ///
    /// What would make this red: the owner or the mode left unchecked on a directory that
    /// already exists.
    #[cfg(unix)]
    #[test]
    fn the_socket_directory_is_refused_unless_it_is_private() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
        let uid = unsafe { libc::geteuid() };
        let tmp = scratch("dir");
        // Whatever the umask made of it: a parent the group can write to is refused below.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let made = control_dir_in(None, &tmp, uid).expect("a fresh directory");
        assert_eq!(made, tmp.join(format!("volant-cm-{uid}")));
        let mode = std::fs::metadata(&made)
            .expect("created")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(control_dir_in(None, &tmp, uid), Ok(made.clone()), "reused");

        std::fs::set_permissions(&made, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(
            control_dir_in(None, &tmp, uid).is_err(),
            "other permissions"
        );
        std::fs::set_permissions(&made, std::fs::Permissions::from_mode(0o700)).expect("chmod");

        // Created by this process, so owned by somebody other than the uid asked about.
        let other = uid + 1;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(tmp.join(format!("volant-cm-{other}")))
            .expect("a directory of the wrong owner");
        let err = control_dir_in(None, &tmp, other).expect_err("another owner");
        assert!(
            err.contains("owned by"),
            "the refusal names the wrong owner"
        );

        let runtime = tmp.join("runtime");
        std::fs::create_dir(&runtime).expect("a runtime directory");
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o775)).expect("chmod");
        assert_eq!(
            control_dir_in(Some(&runtime), &tmp, uid),
            Ok(made.clone()),
            "a runtime directory the group can write to is passed over"
        );
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert_eq!(
            control_dir_in(Some(&runtime), &tmp, uid),
            Ok(runtime.join("volant-cm"))
        );
        let spaced = tmp.join("run time");
        std::fs::create_dir(&spaced).expect("a runtime directory with a space");
        assert_eq!(
            control_dir_in(Some(&spaced), &tmp, uid),
            Ok(made),
            "a path ssh would split is passed over"
        );

        let open = tmp.join("open");
        std::fs::create_dir(&open).expect("a shared parent");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        let err = control_dir_in(None, &open, uid).expect_err("a parent anybody can write to");
        assert!(
            err.contains("not sticky"),
            "the refusal names the missing sticky bit"
        );
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o1777)).expect("chmod");
        assert_eq!(
            control_dir_in(None, &open, uid),
            Ok(open.join(format!("volant-cm-{uid}"))),
            "a sticky parent, as /tmp is"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    struct RestoreLimit(libc::rlimit);

    #[cfg(unix)]
    impl Drop for RestoreLimit {
        fn drop(&mut self) {
            // SAFETY: the struct is only read, and lives for the whole call.
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) };
        }
    }

    #[cfg(unix)]
    fn open_file_limit() -> libc::rlimit {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `getrlimit` writes into the struct it is given, which lives for the call.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        limit
    }

    /// The soft limit is lowered first: on a machine where it already equals the hard limit,
    /// asserting on what is there would pass with the mechanism removed.
    ///
    /// What would make this red: the raise not happening, or stopping short of the hard limit.
    #[cfg(unix)]
    #[test]
    fn the_open_file_limit_is_raised_to_the_hard_limit() {
        let before = open_file_limit();
        let _restore = RestoreLimit(before);
        assert!(
            before.rlim_max > 256,
            "this test needs a hard limit above 256, found {}",
            before.rlim_max
        );
        let low = libc::rlimit {
            rlim_cur: 256,
            rlim_max: before.rlim_max,
        };
        // SAFETY: as above.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) }, 0);
        assert_eq!(open_file_limit().rlim_cur, 256);
        raise_open_file_limit();
        assert_eq!(open_file_limit().rlim_cur, before.rlim_max.min(1 << 20));
    }

    /// A limit raised after the first connections opened would come too late for them.
    ///
    /// What would make this red: the call moved behind `run_all`, or dropped.
    #[test]
    fn the_open_file_limit_is_raised_before_anything_connects() {
        let source = include_str!("cli.rs");
        let body = source
            .split_once("pub fn run(")
            .expect("cli::run is where both binaries start")
            .1;
        let raise = body
            .find("raise_open_file_limit()")
            .expect("cli::run raises the open file limit");
        let run_all = body.find("run_all(&args").expect("cli::run calls run_all");
        assert!(raise < run_all, "the limit is raised before run_all");
    }

    /// Every connection of a run is resolved in the driver, so sharing is applied there, on
    /// each resolution, or not at all.
    ///
    /// What would make this red: a resolution in the driver that skips `shared`.
    #[test]
    fn the_driver_shares_every_connection_it_resolves() {
        let source = include_str!("executor/driver.rs");
        let calls: Vec<&str> = source.split("Transport::for_vars(").skip(1).collect();
        assert!(!calls.is_empty(), "the driver resolves its connections");
        for call in calls {
            let arm: String = call
                .split("Err(")
                .next()
                .unwrap_or(call)
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            assert!(
                arm.contains(
                    ".shared(options.control_dir.as_deref(),&name,delegate_name.as_deref()"
                ),
                "a resolution without sharing, or shared under the delegate's name: {arm}"
            );
        }
    }

    /// A reconnection to a host that went away first stops the host's master, in the two
    /// places one happens: the plugin that rebooted it, and a kept link found dead after a
    /// reboot.
    ///
    /// What would make this red: the master stopped after the connection, or not at all.
    #[test]
    fn a_reconnection_after_a_reboot_starts_from_a_fresh_connection() {
        let source = include_str!("executor/run.rs");
        for (start, end) in [
            (
                "impl Relink<AgentLink> for Relinker",
                "fn with_connect_timeout",
            ),
            ("pub(super) async fn reuse_or_connect", "\n}\n"),
        ] {
            let body = source.split_once(start).expect(start).1;
            let body: String = body[..body.find(end).expect(end)]
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let (stop, reconnect) = if start.contains("Relinker") {
                ("transport.stop_shared().await", "connect(&transport,")
            } else {
                // `check_kept` stops the master on a failed check; its own test proves how.
                ("check_kept(", "connect(&key.transport,")
            };
            let stop = body
                .find(stop)
                .unwrap_or_else(|| panic!("{start} stops the master"));
            let connect = body
                .find(reconnect)
                .unwrap_or_else(|| panic!("{start} reconnects through the transport it stopped"));
            assert!(stop < connect, "{start}: the master is stopped first");
        }
    }
}
