// SPDX-License-Identifier: GPL-3.0-or-later
//! How the controller reaches a host. Every transport ends up as a process whose stdin and
//! stdout carry protocol frames: the agent itself for `local`, an `ssh` running the agent
//! remotely for `ssh`.

mod agent_dir;
mod bootstrap;
mod buildah;
mod choose;
mod chroot;
mod control;
mod copy;
mod defaults;
mod docker;
mod escalation;
mod exec;
mod fused;
mod kubectl;
mod lxd;
mod names;
mod podman;
mod reach;
mod resolve;
mod session;
mod shell;
mod ssh;
mod ssm;
mod tar;
pub mod trace;

pub use bootstrap::{space_needed_kib, triple_for};
pub use chroot::ChrootSettings;
pub use control::{control_dir, raise_open_file_limit};
pub use copy::{CopyInto, Feed};
pub use defaults::{
    BecomeMethod, ConnectError, ConnectionDefaults, ConnectionFailsTask, ConnectionKeywords,
    Escalation, EscalationProgram, HostOwn, INCORRECT_SU_PASSWORD, INCORRECT_SUDO_PASSWORD,
    MISSING_SUDO_PASSWORD, SU_PREMATURE_END, Secret, SshTarget, Transport, plugin_load_warning,
    refused_escalation,
};
pub use docker::{DockerSettings, PluginSettings};
pub use exec::{ExecTarget, KeptFile, Lines};
#[cfg(test)]
pub(crate) use names::all as connection_variables;
pub(crate) use names::{first_set, secret_text};
pub use podman::PodmanSettings;
pub use resolve::{ConnectionNames, Resolution, ResolveConnection};
pub(crate) use shell::validate_remote_tmp;
pub use ssm::SsmTarget;
pub(crate) use trace::shell_quote;

#[cfg(test)]
mod tests;
