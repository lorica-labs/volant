// SPDX-License-Identifier: GPL-3.0-or-later
//! What a plugin knows of the link its task runs over, and the plugin a task runs as on a host
//! no agent runs on.

use crate::vars::HostVars;

use super::Kind;

/// What a plugin reads of the link its module runs over that is not a variable: the settings the
/// link was opened with, which the reference puts on the plugin's play context and connection.
#[derive(Debug, Default, Clone)]
pub(crate) struct LinkFacts {
    /// The ssh key and user the link uses: the host's variable, else the command line or
    /// `ansible.cfg`.
    pub private_key: Option<String>,
    pub remote_user: Option<String>,
    /// The method and the user the link escalates to, when it does.
    pub escalation: Option<(String, String)>,
    /// `ssh_common_args` and `ssh_extra_args` of an ssh link, as one string each.
    pub ssh_common_args: Option<String>,
    pub ssh_extra_args: Option<String>,
    /// The host the task is delegated to, as the plugin reads it off `delegate_to`.
    pub delegate: Option<String>,
    /// Whether the link runs the agent on the controller itself.
    pub local: bool,
    /// The connection plugin the helper reaches the host through, for a host no agent runs on.
    pub owned: Option<String>,
}

impl LinkFacts {
    /// The facts of the link `transport` opens, escalated as `escalation` says.
    pub(crate) fn of(
        transport: &crate::transport::Transport,
        escalation: Option<&crate::transport::Escalation>,
        delegate: Option<&(String, HostVars)>,
    ) -> Self {
        let words = |words: &[String]| {
            (!words.is_empty()).then(|| {
                words
                    .iter()
                    .map(|word| crate::transport::shell_quote(word))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        };
        let mut facts = LinkFacts {
            escalation: escalation.map(|e| (e.method.name().to_string(), e.user.clone())),
            delegate: delegate.map(|(name, _)| name.clone()),
            local: matches!(transport, crate::transport::Transport::Local),
            remote_user: transport.user().map(str::to_string),
            ..LinkFacts::default()
        };
        if let crate::transport::Transport::Python(target) = transport {
            facts.owned = Some(target.plugin.clone());
        }
        if let crate::transport::Transport::Ssh(target) = transport {
            facts.private_key = target
                .own
                .private_key
                .as_ref()
                .map(|k| k.display().to_string());
            facts.ssh_common_args = words(&target.common_args);
            facts.ssh_extra_args = words(&target.extra_args);
        }
        facts
    }
}

/// The plugin a task runs as over `transport`. On a host no agent runs on the helper runs
/// everything: the task's own plugin, ansible-core's of the same name for one this engine drives
/// itself, and `normal` for a module with no plugin of its own.
pub(crate) fn kind_on(transport: &crate::transport::Transport, kind: Option<Kind>) -> Option<Kind> {
    if !matches!(transport, crate::transport::Transport::Python(_)) {
        return kind;
    }
    Some(Kind::Bridged(match kind {
        None => "normal",
        Some(Kind::Bridged(name)) => name,
        Some(Kind::Copy) => "copy",
        Some(Kind::Dnf) => "dnf",
        Some(Kind::Fetch) => "fetch",
        Some(Kind::Package) => "package",
        Some(Kind::Reboot) => "reboot",
        Some(Kind::Service) => "service",
        Some(Kind::Template) => "template",
        Some(Kind::Unarchive) => "unarchive",
    }))
}
