// SPDX-License-Identifier: GPL-3.0-or-later
//! `package`: the host's package manager names the module that runs.
//!
//! Read off `plugins/action/package.py` of ansible-core 2.19.12 and measured (measure 4). The
//! manager is `use:` unless it is `auto`, then the `ansible_package_use` variable, then
//! `ansible_facts.pkg_mgr` of the host the module runs on, then a `setup` filtered to that one
//! fact, run again for every task and never kept.
//!
//! The name a host reports is engine input and is never compiled: it is only looked up in
//! [`modules_for`]'s closed list. The worst a host can do is have another builtin backend run,
//! which the reference lets it do as well, and never a module the run did not put in its union.

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use super::{Context, Kind, Plugin, Step, Sub, fact, gathered, lost, modules_for, setup_for};
use crate::vars::host_setting;

enum State {
    Start,
    Setup,
    Module(&'static str),
}

pub(super) struct Package<'a> {
    args: &'a Map<String, Value>,
    running_vars: &'a Map<String, Value>,
    state: State,
}

impl<'a> Package<'a> {
    pub(super) fn new(ctx: Context<'a>) -> Self {
        Package {
            args: ctx.args,
            running_vars: ctx.running_vars,
            state: State::Start,
        }
    }

    /// The module a manager's name picks, with the arguments less `use`, or the reference's own
    /// refusal when no module of the union carries that name.
    fn dispatch(&mut self, name: &str) -> Step {
        // `setup` is in the union for the question, and is never an answer to it.
        let Some(module) = modules_for(Kind::Package)
            .iter()
            .copied()
            .find(|m| *m != "setup" && *m == short_name(name))
        else {
            return Step::Done(TaskResult::failed_with(format!(
                "Could not find a matching action for the \"{name}\" package manager."
            )));
        };
        let mut args = self.args.clone();
        args.remove("use");
        self.state = State::Module(module);
        Sub::run(module, args)
    }
}

/// A value that names a manager, which `auto` does not.
fn named(value: Option<&str>) -> Option<&str> {
    value.filter(|v| *v != "auto")
}

impl Plugin for Package<'_> {
    fn next(&mut self, last: Option<TaskResult>) -> Step {
        match self.state {
            State::Start => {
                let chosen = named(self.args.get("use").and_then(Value::as_str))
                    .or_else(|| {
                        named(
                            host_setting(self.running_vars, "ansible_package_use")
                                .and_then(Value::as_str),
                        )
                    })
                    .or_else(|| named(fact(self.running_vars, "pkg_mgr")));
                if let Some(name) = chosen {
                    self.dispatch(name)
                } else {
                    self.state = State::Setup;
                    setup_for("ansible_pkg_mgr")
                }
            }
            State::Setup => {
                let Some(mut facts) = last else {
                    return Step::Done(lost("setup"));
                };
                if facts.failed() {
                    let msg = facts
                        .0
                        .get("msg")
                        .and_then(Value::as_str)
                        .unwrap_or("None")
                        .to_string();
                    // The reference fails with the `setup` result under its own sentence. Its
                    // facts go, though: a filtered `setup` is never kept, and a result is where
                    // `record_facts` would find them.
                    facts.0.remove("ansible_facts");
                    facts.0.insert("failed".into(), Value::Bool(true));
                    facts.0.insert(
                        "msg".into(),
                        Value::String(format!(
                            "Failed to fetch ansible_pkg_mgr to determine the package action backend: {msg}"
                        )),
                    );
                    return Step::Done(facts);
                }
                match named(gathered(&facts, "ansible_pkg_mgr")) {
                    Some(name) => self.dispatch(name),
                    None => Step::Done(TaskResult::failed_with(
                        "Could not detect a package manager. Try using the \"use\" option.",
                    )),
                }
            }
            // The module's result is the task's, as it is: measure 4, `p1`.
            State::Module(module) => Step::Done(last.unwrap_or_else(|| lost(module))),
        }
    }
}
