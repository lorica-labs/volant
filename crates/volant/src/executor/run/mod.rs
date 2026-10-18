// SPDX-License-Identifier: GPL-3.0-or-later
//! Running one task and judging its result, on the controller as well as on the agent.

mod argspec;
mod async_job;
mod batch;
mod blob;
mod channel;
mod controller;
mod interpreter;
mod judge;
mod links;
mod local;
mod output;
mod plugin;
mod record;
mod register;
mod retry;

pub(super) use argspec::raw_params_refusal;
pub(super) use async_job::{Polled, poll_job, polls};
pub(super) use batch::run_agent_batch;

pub(super) use channel::Relinker;
pub(super) use controller::pause_lines;
pub(super) use interpreter::{
    DISCOVERED_INTERPRETER, DISCOVERY_HELP, action_fqcn, chosen_interpreter, discovers_interpreter,
    discovery_warning, python_for, requested_interpreter, running_host_vars, silent_discovery,
    stamp_discovery, step_tasks,
};
pub(super) use judge::{break_after, broke_before, empty_loop_result, finish, loops};
pub(super) use links::reuse_or_connect;
pub(super) use local::run_local;
pub(super) use output::classify;
pub(crate) use output::{RAISED_SKIP, drop_internal_keys};
#[cfg(test)]
pub(super) use plugin::protocol_task;
pub(super) use plugin::{PluginStart, run_plugin_attempts, wait_or_stop};
pub(super) use record::{
    fact_targets, record_facts, record_inventory_results, record_local_facts, record_registered,
};
pub(super) use register::{failed_task_value, notified_handlers, registered_value, take_warnings};
pub(super) use retry::{
    Attempt, ItemRetry, RetryLine, any_retried, judge_attempt, local_timed_out, retry_plans,
};

#[cfg(test)]
pub(super) mod test_support;
#[cfg(test)]
mod tests;
