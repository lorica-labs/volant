// SPDX-License-Identifier: GPL-3.0-or-later
//! Connection settings, the escalation description and the types a transport is made of.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use super::names::{self, first_set};
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
    /// `[ssh_connection] usetty` and `ANSIBLE_SSH_USETTY`: the terminal a host gets when it sets
    /// no `ansible_ssh_use_tty` of its own.
    pub ssh_use_tty: bool,
    /// How a connection name other than `local` and `ssh` is resolved: through the controller's
    /// ansible-core when the run can ask it.
    pub names: super::ConnectionNames,
    /// What the collections' connection plugins read from `ansible.cfg` and the environment.
    pub plugins: super::PluginSettings,
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
            .field("ssh_use_tty", &self.ssh_use_tty)
            .field("names", &self.names)
            .field("plugins", &self.plugins)
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

    /// The name a playbook gives it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sudo => "sudo",
            Self::Su => "su",
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
    /// `su`'s `prompt_l10n`: the password prompts it matches instead of the built-in list.
    pub prompt_l10n: Vec<String>,
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
            .field("prompt_l10n", &self.prompt_l10n)
            .finish()
    }
}

pub(super) fn redacted(password: Option<&String>) -> &'static str {
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
/// The reference's text for a `su` that never started the command over a local connection, which
/// follows it with what `su` printed.
pub const SU_PREMATURE_END: &str = "Premature end of stream waiting for become success.";

/// The result of a task whose host refused to escalate, `msg` saying why. Measured for
/// `Incorrect su password` (scout case `c5w`) and, over a local connection, `Premature end of
/// stream`: the reference raises them from its connection, so the result is `changed: false` and
/// a `msg` that already says `Task failed: `. The other refusals keep their shape until they are
/// measured too.
pub fn refused_escalation(msg: String) -> volant_protocol::TaskResult {
    if msg == INCORRECT_SU_PASSWORD || msg.starts_with(SU_PREMATURE_END) {
        let mut shaped = volant_protocol::TaskResult::failed_with(crate::render::task_failed(&msg));
        shaped.0.insert("changed".into(), Value::Bool(false));
        shaped
    } else {
        volant_protocol::TaskResult::failed_with(msg)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Transport {
    /// Run the agent on the controller machine itself.
    Local,
    Ssh(SshTarget),
    /// Run the agent through a command that execs into the target, such as `docker exec -i`.
    Exec(super::ExecTarget),
}

/// A password a host variable gave. It prints as `<redacted>`, so a derived `Debug` on a type
/// that holds one cannot put it in a log or a `-vvvv` line.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Secret(String);

impl Secret {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The settings of a host kept apart from the rest of an [`SshTarget`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostOwn {
    /// `ssh -i`: `ansible_ssh_private_key_file` or `ansible_private_key_file`, else the
    /// command line's or `ansible.cfg`'s.
    pub private_key: Option<PathBuf>,
    /// The connection password a host variable gave. `None` leaves the one of the command line,
    /// if any; an empty one offers no password at all.
    pub password: Option<Secret>,
    /// The `ssh` program, `ssh` unless `ansible_ssh_executable` names another.
    pub executable: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SshTarget {
    pub address: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    /// The key file, the password and the `ssh` program. Boxed so the two variants of a
    /// `Transport` stay close in size.
    pub own: Box<HostOwn>,
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

/// The connection plugins ansible-core 2.19.12 ships besides `local` and `ssh`, with the alias it
/// routes to one of them: names that exist, which this release refuses rather than misreports.
pub(super) const KNOWN_CONNECTIONS: &[&str] = &["paramiko", "paramiko_ssh", "psrp", "winrm"];

/// A connection the task fails for, rather than the host: an `ansible_connection` naming no
/// plugin at all, or a plugin that refuses before it runs anything, as `chroot` refuses a
/// directory that is not one. Measured on ansible-core 2.19.12: the task fails with the message,
/// for a host's own connection, a delegate's and a `run_once` runner's alike, and nobody is
/// unreachable.
#[derive(Debug)]
pub struct ConnectionFailsTask {
    message: String,
    warning: Option<String>,
}

impl std::fmt::Display for ConnectionFailsTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConnectionFailsTask {}

/// The collection ansible-core 2.19.12 routes a connection name to, for the names a collection
/// took over from the core.
const ROUTED_CONNECTIONS: &[(&str, &str)] = &[
    ("buildah", "containers.podman.buildah"),
    ("podman", "containers.podman.podman"),
    ("aws_ssm", "community.aws.aws_ssm"),
    ("chroot", "community.general.chroot"),
    ("docker", "community.docker.docker"),
    ("funcd", "community.general.funcd"),
    ("iocage", "community.general.iocage"),
    ("jail", "community.general.jail"),
    ("kubectl", "kubernetes.core.kubectl"),
    ("libvirt_lxc", "community.libvirt.libvirt_lxc"),
    ("lxc", "community.general.lxc"),
    ("lxd", "community.general.lxd"),
    ("oc", "community.okd.oc"),
    ("qubes", "community.general.qubes"),
    ("saltstack", "community.general.saltstack"),
    ("zone", "community.general.zone"),
    ("vmware_tools", "community.vmware.vmware_tools"),
    ("httpapi", "ansible.netcommon.httpapi"),
    ("napalm", "ansible.netcommon.napalm"),
    ("netconf", "ansible.netcommon.netconf"),
    ("network_cli", "ansible.netcommon.network_cli"),
    ("persistent", "ansible.netcommon.persistent"),
];

/// The warning the reference prints, once, before it fails a task whose connection a missing
/// collection would have supplied: the routed name, and the first package Python cannot import.
pub fn plugin_load_warning(plugin: &str, collection: &str) -> String {
    // `ansible_collections.ansible` exists with the core's own `ansible.builtin`.
    let mut parts = collection.split('.');
    let missing = match (parts.next(), parts.next()) {
        (Some("ansible"), Some(name)) => format!("ansible.{name}"),
        (Some(namespace), _) => namespace.to_string(),
        _ => String::new(),
    };
    format!("Error loading plugin '{plugin}': No module named 'ansible_collections.{missing}'")
}

impl ConnectionFailsTask {
    /// A name no plugin answers to. The warning that goes with it, for a name a collection took
    /// over, is the one the controller's loader gave, else the one the reference gives with no
    /// collection installed.
    pub fn not_found(name: &str, loader_warning: Option<String>) -> ConnectionFailsTask {
        let routed = || {
            let (_, routed) = ROUTED_CONNECTIONS
                .iter()
                .find(|(known, _)| *known == name)?;
            let collection = routed.rsplit_once('.')?.0;
            Some(plugin_load_warning(routed, collection))
        };
        ConnectionFailsTask {
            message: format!("the connection plugin '{name}' was not found"),
            warning: loader_warning.or_else(routed),
        }
    }

    /// A plugin's own refusal, in its words.
    pub fn refused(message: impl Into<String>) -> ConnectionFailsTask {
        ConnectionFailsTask {
            message: message.into(),
            warning: None,
        }
    }

    /// The warning printed before the task fails, when there is one.
    pub fn warning(&self) -> Option<String> {
        self.warning.clone()
    }
}

/// The `remote_user`, `port` and `connection` keywords of one task, after its block and play
/// were folded in and its templates rendered. They sit under the host's own variables and over
/// the command line, which is the order measured on ansible-core 2.19.12.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionKeywords {
    pub user: Option<String>,
    pub port: Option<u16>,
    pub connection: Option<String>,
}

/// The words of one `ansible_ssh_*_args` variable. An unbalanced quote is refused by name
/// rather than dropped (`fallback`, the command line's words, stands when the host sets
/// nothing): silently connecting without a `ProxyJump` or `ProxyCommand` the
/// inventory asked for can reach a different machine than the operator meant.
pub(super) fn setting_args(
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
pub(super) fn as_map(vars: &Map<String, Value>) -> Map<String, Value> {
    vars.clone()
}

/// `ansible_ssh_port` or `ansible_port`, whether the inventory typed it as a number or quoted
/// it as a string.
pub(super) fn port_of(vars: &Map<String, Value>) -> Option<u16> {
    let value = first_set(vars, &names::PORT)?;
    match value {
        Value::Number(_) => value.as_u64().and_then(|p| u16::try_from(p).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `ansible_ssh_timeout` or `ansible_timeout`: the seconds `ssh` waits for the connection, a
/// number or a quoted number. Anything else fails the host by name, the value in the message.
pub(super) fn timeout_of(
    host_name: &str,
    vars: &Map<String, Value>,
) -> anyhow::Result<Option<Duration>> {
    let Some(value) = first_set(vars, &names::TIMEOUT) else {
        return Ok(None);
    };
    let seconds = match value {
        Value::Number(_) => value.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    };
    seconds.map(|seconds| Some(Duration::from_secs(seconds))).ok_or_else(|| {
        anyhow::anyhow!(
            "host '{host_name}': the connection timeout must be a whole number of seconds, found {value}"
        )
    })
}
