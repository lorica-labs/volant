// SPDX-License-Identifier: GPL-3.0-or-later
//! How a link escalates to another user.

mod probe;
mod su;
mod sudo;

#[cfg(test)]
pub(super) use probe::{agent_version_ran, escalation_outcome};
pub(super) use probe::{check_local_escalation, unreachable_message};
pub(super) use su::{settle_form, su_command, su_ended, su_exchange};
pub(super) use sudo::{
    RunAs, SudoForm, become_success, escalated_command, local_argv, preamble, preparing_for,
};
#[cfg(test)]
pub(super) use sudo::{interactive, sudo_prefix};
