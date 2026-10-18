// SPDX-License-Identifier: GPL-3.0-or-later
//! What `free` and `host_pinned` make of the keywords that would have the hosts wait for each
//! other. Measured on ansible-core 2.19.12: `max_fail_percentage`, `any_errors_fatal` and
//! `run_once` are ignored with a warning, and a task whose action bypasses the host loop ends the
//! run.

use crate::compile::Compiled;
use crate::playbook::PlayTask;

/// Printed once a play using either strategy that sets `max_fail_percentage` starts.
pub(super) const MAX_FAIL: &str = "Using max_fail_percentage with the free strategy is not supported, as tasks are executed independently on each host";
const FATAL: &str = "Using any_errors_fatal with the free strategy is not supported, as tasks are executed independently on each host";
const RUN_ONCE: &str = "Using run_once with the free strategy is not currently supported. This task will still be executed for every host in the inventory list.";

/// Clears `any_errors_fatal` and `run_once` on every step and handler of `c`, so every host runs
/// each task on its own and one host's failure ends nobody else's play, and returns the
/// warnings the reference prints for them, in the order the tasks come.
///
/// The reference warns when a host reaches such a task; here the warnings come when the steps
/// are loaded, so a task no host reaches still warns. A failed `run_once` task fails only its
/// own host here, where the reference fails every host of the play.
pub(super) fn loosen(c: &mut Compiled) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    let tasks = c.steps.iter_mut().map(|s| &mut s.task);
    for task in tasks.chain(c.handlers.iter_mut().map(|h| &mut h.task)) {
        let fatal = task.any_errors_fatal.take() == Some(true);
        let once = task.run_once.take() == Some(true);
        for (hit, text) in [(fatal, FATAL), (once, RUN_ONCE)] {
            if hit && !warnings.iter().any(|w| w == text) {
                warnings.push(text.to_string());
            }
        }
    }
    warnings
}

/// The error that ends the run when a host reaches `task` under `free` or `host_pinned` and its
/// action bypasses the host loop: `pause` and `add_host`. Printed as the reference prints it,
/// with the task's place in its file below.
pub(super) fn bypass_error(task: &PlayTask) -> Option<String> {
    if !task.pauses() && !task.adds_host() {
        return None;
    }
    let why = format!(
        "The '{}' module bypasses the host loop, which is currently not supported in the free strategy and would instead execute for every host in the inventory list.",
        task.module
    );
    Some(match &task.origin {
        Some(origin) => format!("{why}\n{}", crate::render::source_context(origin)),
        None => why,
    })
}

#[cfg(test)]
mod tests;
