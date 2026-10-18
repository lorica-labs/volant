// SPDX-License-Identifier: GPL-3.0-or-later
//! How the controller reaches a host. Every transport ends up as a process whose stdin and
//! stdout carry protocol frames: the agent itself for `local`, an `ssh` running the agent
//! remotely for `ssh`.

mod bootstrap;
mod choose;
mod control;
mod defaults;
mod escalation;
mod session;
mod shell;
mod ssh;
pub mod trace;

pub use bootstrap::{space_needed_kib, triple_for};
pub use control::{control_dir, raise_open_file_limit};
pub use defaults::{
    BecomeMethod, ConnectError, ConnectionDefaults, ConnectionKeywords, ConnectionNotFound,
    Escalation, EscalationProgram, INCORRECT_SU_PASSWORD, INCORRECT_SUDO_PASSWORD,
    MISSING_SUDO_PASSWORD, SshTarget, Transport,
};
pub(crate) use shell::validate_remote_tmp;

#[cfg(test)]
mod tests;
