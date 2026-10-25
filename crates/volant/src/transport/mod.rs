// SPDX-License-Identifier: GPL-3.0-or-later
//! How the controller reaches a host. Every transport ends up as a process whose stdin and
//! stdout carry protocol frames: the agent itself for `local`, an `ssh` running the agent
//! remotely for `ssh`.

mod agent_dir;
mod bootstrap;
mod choose;
mod control;
mod defaults;
mod docker;
mod escalation;
mod exec;
mod fused;
mod names;
mod reach;
mod resolve;
mod session;
mod shell;
mod ssh;
pub mod trace;

pub use bootstrap::{space_needed_kib, triple_for};
pub use control::{control_dir, raise_open_file_limit};
pub use defaults::{
    BecomeMethod, ConnectError, ConnectionDefaults, ConnectionKeywords, ConnectionNotFound,
    Escalation, EscalationProgram, HostOwn, INCORRECT_SU_PASSWORD, INCORRECT_SUDO_PASSWORD,
    MISSING_SUDO_PASSWORD, SU_PREMATURE_END, Secret, SshTarget, Transport, plugin_load_warning,
    refused_escalation,
};
pub use docker::DockerSettings;
pub use exec::ExecTarget;
#[cfg(test)]
pub(crate) use names::all as connection_variables;
pub(crate) use names::{first_set, secret_text};
pub use resolve::{ConnectionNames, Resolution, ResolveConnection};
pub(crate) use shell::validate_remote_tmp;
pub(crate) use trace::shell_quote;

#[cfg(test)]
mod tests;
