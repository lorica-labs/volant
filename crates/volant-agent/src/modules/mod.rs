// SPDX-License-Identifier: GPL-3.0-or-later
//! Native modules. Each one takes Ansible's argument names and returns Ansible's result shape.

pub mod command;

use volant_protocol::{Task, TaskResult};

/// Outcome of running one task.
pub enum Run {
    Done(TaskResult),
    /// The controller cancelled the batch while this task was running.
    Cancelled,
}

/// Runs a task with the native module that matches its name.
pub fn run(task: &Task, cancelled: &dyn Fn() -> bool) -> Run {
    let name = task
        .module
        .strip_prefix("ansible.builtin.")
        .or_else(|| task.module.strip_prefix("ansible.legacy."))
        .unwrap_or(&task.module);
    match name {
        "command" => command::run(&task.args, false, cancelled),
        "shell" | "raw" => command::run(&task.args, true, cancelled),
        other => Run::Done(TaskResult::failed_with(format!(
            "The module {other} is not available on the agent yet"
        ))),
    }
}
