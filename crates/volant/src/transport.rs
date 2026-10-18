// SPDX-License-Identifier: GPL-3.0-or-later
//! How the controller reaches a host. Every transport ends up as a process whose stdin and
//! stdout carry protocol frames: the agent itself for `local`, an `ssh` running the agent
//! remotely for `ssh`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::bail;
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::agent::{AgentLink, AgentSource};
use crate::inventory::Host;
use crate::vars::host_setting;

/// Connection settings from `ansible.cfg` and the command line; host variables override them.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionDefaults {
    pub remote_user: Option<String>,
    /// The `-c` plugin name; `ansible_connection` and the `connection:` keyword speak over it.
    pub connection: Option<String>,
    pub private_key: Option<PathBuf>,
    pub host_key_checking: bool,
    pub remote_tmp: String,
    pub connect_timeout: Duration,
    /// Whether a task escalates when neither the play nor the host says otherwise.
    pub r#become: bool,
    /// Who to become, Ansible's `become_user`, `root` unless configured otherwise.
    pub become_user: String,
    /// `sudo` or `su`; anything else is refused before a playbook runs.
    pub become_method: String,
    /// `become_exe` and `become_flags` from `ansible.cfg` or the environment, as written.
    pub become_exe: Option<String>,
    pub become_flags: Option<String>,
    /// The `--ask-become-pass` answer, if one was asked for.
    pub become_password: Option<String>,
    /// `--ssh-common-args` and `--ssh-extra-args`, already split into words; a host's own
    /// `ansible_ssh_common_args` or `ansible_ssh_extra_args` replaces the one it names.
    pub ssh_common_args: Vec<String>,
    pub ssh_extra_args: Vec<String>,
}

/// Written by hand so `become_password` cannot reach a log or a `-vvv` dump: a derived `Debug`
/// on this struct would print the operator's password every time the connection defaults are
/// formatted.
impl std::fmt::Debug for ConnectionDefaults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionDefaults")
            .field("remote_user", &self.remote_user)
            .field("connection", &self.connection)
            .field("private_key", &self.private_key)
            .field("host_key_checking", &self.host_key_checking)
            .field("remote_tmp", &self.remote_tmp)
            .field("connect_timeout", &self.connect_timeout)
            .field("become", &self.r#become)
            .field("become_user", &self.become_user)
            .field("become_method", &self.become_method)
            .field("become_exe", &self.become_exe)
            .field("become_flags", &self.become_flags)
            .field("become_password", &redacted(self.become_password.as_ref()))
            .field("ssh_common_args", &self.ssh_common_args)
            .field("ssh_extra_args", &self.ssh_extra_args)
            .finish()
    }
}

/// The escalation programs this release drives, by the name a playbook gives them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BecomeMethod {
    Sudo,
    Su,
}

impl BecomeMethod {
    /// The method a `become_method` names, or `None` for one this release does not implement.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "sudo" => Some(Self::Sudo),
            "su" => Some(Self::Su),
            _ => None,
        }
    }
}

/// Privilege escalation for one link: run the agent as `user` through `sudo` or `su`.
#[derive(Clone, PartialEq, Eq)]
pub struct Escalation {
    pub method: BecomeMethod,
    pub user: String,
    pub password: Option<String>,
    /// `become_exe`, split into shell words; empty runs the method's own program.
    pub exe: Vec<String>,
    /// `become_flags`, split into shell words; `None` keeps the method's own default.
    pub flags: Option<Vec<String>>,
}

/// How a link escalates, less the user and the password: two links to one user through
/// different programs or flags are different agents, started under different rules. The
/// password is left out on purpose, so a password that changes in the middle of a play keeps
/// the link it already has, the escalation it proved being the same.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EscalationProgram {
    pub method: BecomeMethod,
    pub exe: Vec<String>,
    pub flags: Option<Vec<String>>,
}

impl Escalation {
    /// What of this escalation keys its link, beside the user.
    pub fn program(&self) -> EscalationProgram {
        EscalationProgram {
            method: self.method,
            exe: self.exe.clone(),
            flags: self.flags.clone(),
        }
    }
}

/// Also written by hand, and for the same reason: `Escalation` travels through the executor,
/// where a derived `Debug` would put the password in any diagnostic that formats a batch.
impl std::fmt::Debug for Escalation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Escalation")
            .field("method", &self.method)
            .field("user", &self.user)
            .field("password", &redacted(self.password.as_ref()))
            .field("exe", &self.exe)
            .field("flags", &self.flags)
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
/// The reference's text for a `su` that refused the password, or asked for it a second time.
pub const INCORRECT_SU_PASSWORD: &str = "Incorrect su password";

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
    /// `ansible_ssh_use_tty`: whether a command sent with no stdin runs on a terminal, as the
    /// reference's `ssh -tt` gives it. On unless the host turns it off.
    pub use_tty: bool,
    /// What the `ssh -G` that `SshTarget::resolved` ran said: the `hostname` it printed and
    /// whether the operator's configuration already shares the connection. `None` when no dump
    /// was taken, which the closing line of a terminal then asks for on first use.
    pub dumped: Option<Box<(Option<String>, bool)>>,
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

/// The connection plugins ansible-core 2.19.12 ships besides `local` and `ssh`, with the alias it
/// routes to one of them: names that exist, which this release refuses rather than misreports.
const KNOWN_CONNECTIONS: &[&str] = &["paramiko", "paramiko_ssh", "psrp", "winrm"];

/// An `ansible_connection` naming no plugin at all. Measured on ansible-core 2.19.12: the task
/// fails with this message, for a host's own connection, a delegate's and a `run_once`
/// runner's alike, and nobody is unreachable.
#[derive(Debug)]
pub struct ConnectionNotFound(pub String);

impl std::fmt::Display for ConnectionNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the connection plugin '{}' was not found", self.0)
    }
}

impl std::error::Error for ConnectionNotFound {}

/// The `remote_user`, `port` and `connection` keywords of one task, after its block and play
/// were folded in and its templates rendered. They sit under the host's own variables and over
/// the command line, which is the order measured on ansible-core 2.19.12.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionKeywords {
    pub user: Option<String>,
    pub port: Option<u16>,
    pub connection: Option<String>,
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
        Self::for_task(host_name, vars, defaults, &ConnectionKeywords::default())
    }

    /// [`Transport::for_vars`] for a task that carries connection keywords: a host variable
    /// beats the task's keyword, which beats the command line and `ansible.cfg`.
    pub fn for_task(
        host_name: &str,
        vars: &Map<String, Value>,
        defaults: &ConnectionDefaults,
        keywords: &ConnectionKeywords,
    ) -> anyhow::Result<Transport> {
        let setting_text = |key: &str| {
            host_setting(vars, key)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let connection = setting_text("ansible_connection")
            .or_else(|| keywords.connection.clone())
            .or_else(|| defaults.connection.clone());
        match connection.as_deref().unwrap_or("ssh") {
            "local" => Ok(Transport::Local),
            "ssh" => Ok(Transport::Ssh(SshTarget {
                address: setting_text("ansible_host").unwrap_or_else(|| host_name.to_string()),
                port: port_of(vars).or(keywords.port),
                user: setting_text("ansible_user")
                    .or_else(|| keywords.user.clone())
                    .or_else(|| defaults.remote_user.clone()),
                private_key: setting_text("ansible_ssh_private_key_file")
                    .map(PathBuf::from)
                    .or_else(|| defaults.private_key.clone()),
                common_args: setting_args(
                    host_name,
                    vars,
                    "ansible_ssh_common_args",
                    &defaults.ssh_common_args,
                )?,
                extra_args: setting_args(
                    host_name,
                    vars,
                    "ansible_ssh_extra_args",
                    &defaults.ssh_extra_args,
                )?,
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
                dumped: None,
                use_tty: host_setting(vars, "ansible_ssh_use_tty").is_none_or(
                    |value| match value {
                        Value::Bool(on) => *on,
                        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
                        // The reference reads a word it does not know as false.
                        Value::String(text) => {
                            crate::yaml::bool_from_str(text.trim()).unwrap_or(false)
                        }
                        other => crate::template::truthy(other),
                    },
                ),
            })),
            // ansible-core's own plugins this release does not drive, and a collection's, are
            // refused; any other name is no plugin at all, which the reference fails the task for.
            other if KNOWN_CONNECTIONS.contains(&other) || other.contains('.') => {
                bail!("host '{host_name}': connection '{other}' is not supported")
            }
            other => Err(ConnectionNotFound(other.to_string()).into()),
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
                    // `su` reads its password from a terminal, which this side only reaches
                    // through `ssh -tt`; a local link has none to give it.
                    Some(e) if e.method == BecomeMethod::Su => {
                        return Err(ConnectError::Become(
                            "become_method 'su' is not supported yet on a local connection"
                                .to_string(),
                        ));
                    }
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

/// `sudo`'s words up to `-u`: `become_exe` or `sudo`, then `become_flags` or `-H`, then the
/// probed form's own. The reference's default flags are `-H -S -n`; the last two are what the
/// probed form decides here, so only `-H` stands in for them.
///
/// The form that writes a password drops `-n` from `become_flags` first, as the reference does
/// when it has a password to send: a `sudo` told never to ask refuses the line it is given.
fn sudo_words(escalation: &Escalation, form: SudoForm) -> Vec<String> {
    let mut words = if escalation.exe.is_empty() {
        vec!["sudo".to_string()]
    } else {
        escalation.exe.clone()
    };
    match &escalation.flags {
        None => words.push("-H".to_string()),
        Some(flags) if form == SudoForm::ReadStdin => {
            words.extend(flags.iter().filter_map(|flag| interactive(flag)));
        }
        Some(flags) => words.extend(flags.iter().cloned()),
    }
    words.extend(form.flags().iter().map(ToString::to_string));
    words
}

/// One `become_flags` word with its non-interactive switch taken out, the way sudo.py rewrites
/// it: `-n` and `--non-interactive` go, and a cluster of short flags loses its `n`
/// (its `^(-\w*)n(\w*.*)` pattern, whose greedy `\w*` finds the last `n` of the leading run
/// of word characters).
fn interactive(flag: &str) -> Option<String> {
    if flag == "-n" || flag == "--non-interactive" {
        return None;
    }
    let Some(cluster) = flag.strip_prefix('-').filter(|rest| !rest.starts_with('-')) else {
        return Some(flag.to_string());
    };
    let run = cluster
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(cluster.len());
    Some(match cluster[..run].rfind('n') {
        Some(at) => format!("-{}{}", &cluster[..at], &cluster[at + 1..]),
        None => flag.to_string(),
    })
}

/// The command line that starts a local agent, under `sudo` when escalation is asked for.
fn local_argv(agent: &str, escalated: Escalated<'_>) -> Vec<String> {
    let Some((escalation, form)) = escalated else {
        return vec![agent.to_string()];
    };
    let mut argv = sudo_words(escalation, form);
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
    let words = sudo_words(escalation, form)
        .iter()
        .map(|word| single_quoted(word))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{words} -u {} -- ", single_quoted(&escalation.user))
}

/// Who a bootstrap command runs as: the connecting user, or the escalated one through `sudo`
/// in its settled form, both over plain pipes; or the escalated one through `su`, over a
/// terminal.
#[derive(Clone, Copy)]
enum RunAs<'a> {
    Direct(Escalated<'a>),
    Su(&'a Escalation),
}

/// A bootstrap failure on an escalated link, said to be about that user's agent.
fn preparing_for(err: ConnectError, escalation: Option<&Escalation>) -> ConnectError {
    match (err, escalation) {
        (ConnectError::Unreachable(msg), Some(e)) => {
            ConnectError::Unreachable(format!("preparing the agent for user {}: {msg}", e.user))
        }
        (err, _) => err,
    }
}

/// The line the command behind `su` prints once `su` has let it in and its terminal is raw:
/// everything before it is `su` talking, everything after it is the command's own. A fresh one
/// for every `su`, like the reference's `BECOME-SUCCESS-<random>`, so nothing printed before
/// `su` lets the command in can pass for it.
fn become_success() -> String {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seed = format!(
        "{:?} {} {}",
        std::time::SystemTime::now(),
        std::process::id(),
        COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    format!(
        "VOLANT-BECOME-SUCCESS-{}",
        &blake3::hash(seed.as_bytes()).to_hex()[..32]
    )
}

/// The most `su` may print before its success line: a prompt and a refusal are a few dozen
/// bytes, and something printing without end is not `su` asking for anything.
const SU_TALK_LIMIT: usize = 4096;

/// `su`'s password prompts, the reference's `SU_PROMPT_LOCALIZATIONS` of ansible-core 2.19.12,
/// matched as su.py matches them: any of them, then optional spaces, then an ASCII or a
/// full-width colon, in any case.
fn su_prompt() -> &'static regex::Regex {
    static PROMPT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PROMPT.get_or_init(|| {
        const PROMPTS: [&str; 32] = [
            "Password",
            "암호",
            "パスワード",
            "Adgangskode",
            "Contraseña",
            "Contrasenya",
            "Hasło",
            "Heslo",
            "Jelszó",
            "Lösenord",
            "Mật khẩu",
            "Mot de passe",
            "Parola",
            "Parool",
            "Pasahitza",
            "Passord",
            "Passwort",
            "Salasana",
            "Sandi",
            "Senha",
            "Wachtwoord",
            "ססמה",
            "Лозинка",
            "Парола",
            "Пароль",
            "गुप्तशब्द",
            "शब्दकूट",
            "సంకేతపదము",
            "හස්පදය",
            "密码",
            "密碼",
            "口令",
        ];
        let alternatives = PROMPTS.map(regex::escape).join("|");
        regex::Regex::new(&format!(r"(?i)(?:{alternatives})\s*[:：]"))
            .expect("the prompt pattern is fixed")
    })
}

/// The remote command that runs `command` behind `su`, as the reference builds it: `become_exe`
/// or `su`, `become_flags` (none by default, so no `-` login), the user, then `-c` and the
/// command for `/bin/sh` whatever the user's own shell is. Every word is quoted for the remote
/// shell.
///
/// Before `command`, the shell `su` started makes its terminal raw: the agent's frames are
/// binary, and a terminal left as it is would echo them, turn their carriage returns into new
/// lines and read some of their bytes as signals. Only then does it print `success` (see
/// [`become_success`]), so nothing is sent before the terminal is ready for it.
fn su_command(escalation: &Escalation, command: &str, success: &str) -> String {
    let inner = format!("stty raw -echo -iexten || exit 1; echo {success}; {command}");
    let program = if escalation.exe.is_empty() {
        vec!["su".to_string()]
    } else {
        escalation.exe.clone()
    };
    let words = program
        .iter()
        .chain(escalation.flags.iter().flatten())
        .chain(std::iter::once(&escalation.user))
        .map(|word| single_quoted(word))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "exec {words} -c {}",
        single_quoted(&format!("/bin/sh -c {}", single_quoted(&inner)))
    )
}

/// Talks `su` through to the command it guards, the way the reference's ssh connection does:
/// a prompt is answered with the password once, and a second prompt or an `Authentication
/// failure` line is the wrong password. Without a password no prompt is answered, so `su` waits
/// and the link times out, which is what the reference reports too.
///
/// `success` is taken before any prompt as well, as the reference takes it: a connecting user
/// who is already root, or a PAM that lets this one in, gets no prompt at all. Being fresh for
/// every `su`, it can only come from the shell `su` started.
///
/// Reads one byte at a time, so nothing the command prints after `success` is taken off the
/// stream. `Ok(None)` is the command running; `Ok(Some(said))` is the stream ending before it
/// did, with what `su` had said, for the caller to tell a dead connection from a refusal by the
/// exit code.
async fn su_exchange<R, W>(
    out: &mut R,
    input: &mut W,
    password: Option<&str>,
    success: &str,
    timeout: Duration,
) -> Result<Option<String>, ConnectError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut said: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    let mut answered = false;
    let text = |bytes: &[u8]| redacted_stderr(&String::from_utf8_lossy(bytes), password);
    loop {
        let byte = match tokio::time::timeout(timeout, out.read_u8()).await {
            Ok(Ok(byte)) => byte,
            Ok(Err(_)) => {
                said.extend_from_slice(&line);
                return Ok(Some(text(&said)));
            }
            Err(_) => {
                // The reference's own words, with the complete lines `su` printed before it
                // went quiet; a prompt it is still waiting at has no line end and stays out.
                let msg = format!(
                    "Task failed: Timeout ({}s) waiting for privilege escalation prompt: {}",
                    timeout.as_secs(),
                    text(&said).trim()
                );
                return Err(ConnectError::Unreachable(msg.trim_end().to_string()));
            }
        };
        if said.len() + line.len() >= SU_TALK_LIMIT {
            return Err(ConnectError::Become(format!(
                "su printed more than {SU_TALK_LIMIT} bytes without starting the command: {}",
                first_words(&text(&said), "")
            )));
        }
        if byte == b'\n' {
            let complete = String::from_utf8_lossy(&line);
            // Ends with rather than equals: util-linux 2.41's `su` opens the session with an
            // `OSC 3008` context sequence that has no line end of its own, measured on the
            // development machine, and the reference too looks for the marker within the line.
            if complete.trim_end().ends_with(success) {
                return Ok(None);
            }
            if complete.contains("Authentication failure") {
                return Err(ConnectError::Become(INCORRECT_SU_PASSWORD.to_string()));
            }
            said.append(&mut line);
            said.push(b'\n');
            continue;
        }
        line.push(byte);
        // A prompt ends at its colon, the ASCII one or the last byte of the full-width one.
        let Some(password) = password.filter(|_| matches!(byte, b':' | 0x9A)) else {
            continue;
        };
        if !su_prompt().is_match(&String::from_utf8_lossy(&line)) {
            continue;
        }
        if answered {
            return Err(ConnectError::Become(INCORRECT_SU_PASSWORD.to_string()));
        }
        // A `su` that is gone by now closes the terminal, and the read above says so.
        let _ = input.write_all(format!("{password}\n").as_bytes()).await;
        let _ = input.flush().await;
        answered = true;
        line.clear();
    }
}

/// Why a command behind `su` never started, once the stream has ended: `ssh`'s own failure when
/// it exited 255, otherwise what `su` said, with the password taken back out.
fn su_ended(code: Option<i32>, said: &str, stderr: &str, password: Option<&str>) -> ConnectError {
    if code == Some(EXIT_SSH_FAILURE) {
        return ConnectError::Unreachable(unreachable_message(stderr, password));
    }
    ConnectError::Become(first_words(said, "su exited without starting the command"))
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
/// rather than dropped (`fallback`, the command line's words, stands when the host sets
/// nothing): silently connecting without a `ProxyJump` or `ProxyCommand` the
/// inventory asked for can reach a different machine than the operator meant.
fn setting_args(
    host_name: &str,
    vars: &Map<String, Value>,
    key: &str,
    fallback: &[String],
) -> anyhow::Result<Vec<String>> {
    if host_setting(vars, key).is_none() {
        return Ok(fallback.to_vec());
    }
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

/// The machine an `ssh -G` dump says the connection goes to.
fn dump_hostname(dump: &str) -> Option<String> {
    dump.lines()
        .find_map(|line| line.strip_prefix("hostname "))
        .map(str::to_string)
}

/// What `ssh -G` prints for this target: its configuration as `ssh` resolves it, without
/// connecting. `None` when `ssh -G` cannot run or fails.
async fn ssh_dump(bare: &SshTarget) -> Option<String> {
    let mut argv = bare.ssh_argv(":");
    argv.insert(1, "-G".into());
    match Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
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
        let mut argv = vec![SSH_PROGRAM.to_string()];
        if compress {
            argv.push("-C".into());
        }
        argv.extend([
            "-o".to_string(),
            // A password given with `-k` or a password file comes through `SSH_ASKPASS`, which
            // `BatchMode` would switch off.
            if crate::askpass::installed() {
                "BatchMode=no"
            } else {
                "BatchMode=yes"
            }
            .into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout.as_secs()),
        ]);
        if crate::askpass::installed() {
            // One wrong answer is the end of it, as in the reference, not three tries.
            argv.extend(["-o".into(), "NumberOfPasswordPrompts=1".into()]);
        }
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

    /// Reads the agent from stdin into the cache, atomically, after checking free space. It
    /// reads the byte count it was told rather than up to the end of its input, because behind
    /// `su` that input is a terminal, where no end ever comes; `head -c` has that on every host
    /// an agent is built for (GNU coreutils, BusyBox and toybox alike). The byte count is
    /// verified before the temporary copy is made executable and renamed, so a
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
             head -c {size} > \"$t\" || {{ rm -f \"$t\"; exit 1; }}; \
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
        let resolved = self.resolved().await;
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
        let child = Command::new(SSH_PROGRAM)
            .args(&self.ssh_argv(&remote)[1..])
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
        if let Some(said) =
            su_exchange(&mut stdout, &mut stdin, password, &success, timeout).await?
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
    async fn run_su(
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
        if let Some(said) =
            su_exchange(&mut stdout, &mut stdin, password, &success, timeout).await?
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

    /// One bootstrap command, run as the escalated user where the link escalates.
    ///
    /// The `sudo` password goes on stdin ahead of any payload, never after it: with the
    /// `ReadStdin` form `sudo` reads exactly one line before the command it starts sees
    /// anything, so an upload whose first line went to `sudo` would land one line short.
    async fn run_bootstrap(
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
    async fn bootstrap(
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
mod tests;
