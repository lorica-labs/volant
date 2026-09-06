// SPDX-License-Identifier: GPL-3.0-or-later
//! Native modules. Each one takes Ansible's argument names and returns Ansible's result shape.
//! The list of names lives in `volant_protocol::modules`; this table maps each name to code.

pub mod command;
pub mod raw;
pub mod shell;

use std::time::Duration;

use serde_json::{Map, Value};
use volant_protocol::modules::{ModuleSpec, short_name};
use volant_protocol::{Task, TaskResult};

/// Outcome of running one task.
pub enum Run {
    Done(TaskResult),
    /// The controller cancelled the batch while this task was running.
    Cancelled,
}

/// The function signature every native module implements.
type ModuleFn = fn(&Map<String, Value>, Option<Duration>, &dyn Fn() -> bool) -> Run;

/// A native module: its shared spec and the function that runs it.
pub struct Module {
    pub spec: &'static ModuleSpec,
    pub run: ModuleFn,
}

pub const MODULES: &[Module] = &[command::MODULE, raw::MODULE, shell::MODULE];

/// Runs a task with the native module that matches its name.
pub fn run(task: &Task, cancelled: &dyn Fn() -> bool) -> Run {
    let name = short_name(&task.module);
    let timeout = task.timeout.map(Duration::from_secs);
    match MODULES.iter().find(|m| m.spec.name == name) {
        Some(module) => (module.run)(&task.args, timeout, cancelled),
        None => Run::Done(TaskResult::failed_with(format!(
            "The module {name} is not available on the agent yet"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use volant_protocol::modules::NATIVE_MODULES;

    use super::*;

    #[test]
    fn every_registered_module_is_implemented_and_nothing_more() {
        let registered: Vec<&str> = NATIVE_MODULES.iter().map(|m| m.name).collect();
        let implemented: Vec<&str> = MODULES.iter().map(|m| m.spec.name).collect();
        assert_eq!(implemented, registered);
    }
}
