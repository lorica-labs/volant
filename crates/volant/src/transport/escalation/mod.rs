// SPDX-License-Identifier: GPL-3.0-or-later
//! How a link escalates to another user.

mod local_su;
mod local_sudo;
mod probe;
mod su;
mod sudo;

pub(super) use local_su::local_su;
pub(super) use local_sudo::local_sudo;
#[cfg(test)]
pub(super) use probe::escalation_outcome;
pub(super) use probe::unreachable_message;
pub(super) use su::{refuse_option_user, settle_form, su_command, su_ended, su_exchange};
pub(super) use sudo::{
    RunAs, SudoForm, become_success, escalated_command, preamble, preparing_for,
};
#[cfg(test)]
pub(super) use sudo::{interactive, sudo_prefix};
