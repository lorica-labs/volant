// SPDX-License-Identifier: GPL-3.0-or-later
//! `dnf`: the host's package manager picks the `dnf` or the `dnf5` module.
//!
//! Read off `plugins/action/dnf.py` of ansible-core 2.19.12 and measured against it on a host
//! that runs `apt`. The backend is `use`, else `use_backend`, else `auto`. `auto` and `yum` read
//! `ansible_facts.pkg_mgr` of the host the module runs on, and a value that is not one of
//! [`VALID_BACKENDS`] then asks a `setup` filtered to `ansible_pkg_mgr`. Unlike `package`, the
//! reference keeps that answer: when the task is not delegated, its result carries
//! `ansible_facts: {pkg_mgr: <answer>}`, which enters the host's facts the way any module's
//! facts do, as the host's own words.
//!
//! The host's answer only picks between the two module names written in [`backend`], and is
//! never compiled.

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use super::{Context, Plugin, Step, Sub, fact, gathered, lost, setup_for};

/// What the reference takes as the name of a `dnf` backend.
const VALID_BACKENDS: [&str; 5] = ["yum", "yum4", "dnf", "dnf4", "dnf5"];

/// The module a valid backend runs. `dnf5` is its own; the reference turns `yum4` and `dnf4`
/// into `dnf`, and runs `yum` through the redirect ansible-core keeps for the removed module.
fn backend(name: &str) -> Option<&'static str> {
    match name {
        "dnf5" => Some("dnf5"),
        _ if VALID_BACKENDS.contains(&name) => Some("dnf"),
        _ => None,
    }
}

enum State {
    Start,
    Setup,
    Module(&'static str),
}

pub(super) struct Dnf<'a> {
    args: &'a Map<String, Value>,
    running_vars: &'a Map<String, Value>,
    delegated: bool,
    state: State,
    /// What the `setup` answered, which the task's result carries.
    pkg_mgr: Option<String>,
}

impl<'a> Dnf<'a> {
    pub(super) fn new(ctx: Context<'a>) -> Self {
        Dnf {
            args: ctx.args,
            running_vars: ctx.running_vars,
            delegated: ctx.delegated,
            state: State::Start,
            pkg_mgr: None,
        }
    }

    /// The module a backend's name picks, with the arguments less `use` and `use_backend`, or
    /// the reference's two sentences when it names none. A name that is not a string is none.
    fn dispatch(&mut self, name: Option<&str>) -> Step {
        let Some(module) = name.and_then(backend) else {
            let mut failed = self.carrying(TaskResult(Map::new()));
            failed.0.insert("failed".into(), Value::Bool(true));
            // A tuple in the reference, and so a list; the closing `}` is in its source.
            failed.0.insert(
                "msg".into(),
                json!([
                    "Could not detect which major revision of dnf is in use, which is required to determine module backend.",
                    "You should manually specify use_backend to tell the module whether to use the dnf4 or dnf5 backend})",
                ]),
            );
            return Step::Done(failed);
        };
        let mut args = self.args.clone();
        args.remove("use");
        args.remove("use_backend");
        self.state = State::Module(module);
        Sub::run(module, args)
    }

    /// `result` with the `setup`'s answer under `ansible_facts`, unless it has facts of its own:
    /// the reference updates its result with the module's, so the module's win.
    fn carrying(&self, mut result: TaskResult) -> TaskResult {
        if let Some(name) = &self.pkg_mgr {
            result
                .0
                .entry("ansible_facts")
                .or_insert_with(|| json!({ "pkg_mgr": name }));
        }
        result
    }
}

impl Plugin for Dnf<'_> {
    fn next(&mut self, last: Option<TaskResult>) -> Step {
        match self.state {
            State::Start => {
                if self.args.contains_key("use") && self.args.contains_key("use_backend") {
                    return Step::Done(TaskResult::failed_with(
                        "parameters are mutually exclusive: ('use', 'use_backend')",
                    ));
                }
                let asked = match self
                    .args
                    .get("use")
                    .or_else(|| self.args.get("use_backend"))
                {
                    None => Some("auto"),
                    Some(value) => value.as_str(),
                };
                if !matches!(asked, Some("auto" | "yum")) {
                    return self.dispatch(asked);
                }
                let name = fact(self.running_vars, "pkg_mgr").or(asked);
                if name.and_then(backend).is_some() {
                    return self.dispatch(name);
                }
                self.state = State::Setup;
                setup_for("ansible_pkg_mgr")
            }
            State::Setup => {
                let Some(facts) = last else {
                    return Step::Done(lost("setup"));
                };
                let name = gathered(&facts, "ansible_pkg_mgr").filter(|n| *n != "auto");
                // A delegate's answer would be filed under the host the task was written for,
                // which is not the host that gave it.
                if !self.delegated {
                    self.pkg_mgr = name.map(str::to_string);
                }
                self.dispatch(name)
            }
            State::Module(module) => {
                Step::Done(self.carrying(last.unwrap_or_else(|| lost(module))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A delegated task's result carries no `pkg_mgr`: the reference files it under the delegate
    /// only with `delegate_facts`, and this engine files a result's facts under the host the task
    /// was written for, so the delegate's answer would land on the wrong host.
    ///
    /// What would make this red: the answer carried whoever gave it, which tells the host the
    /// task was written for that it runs the delegate's package manager.
    #[test]
    fn a_delegated_task_carries_no_pkg_mgr() {
        let args = Map::new();
        let running = Map::new();
        for (delegated, facts) in [(false, Some(json!({"pkg_mgr": "dnf5"}))), (true, None)] {
            let mut plugin = Dnf {
                args: &args,
                running_vars: &running,
                delegated,
                state: State::Start,
                pkg_mgr: None,
            };
            assert!(matches!(
                plugin.next(None),
                Step::Run(Sub {
                    module: "setup",
                    ..
                })
            ));
            let setup = TaskResult(
                json!({"ansible_facts": {"ansible_pkg_mgr": "dnf5"}})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            assert!(matches!(
                plugin.next(Some(setup)),
                Step::Run(Sub { module: "dnf5", .. })
            ));
            let Step::Done(result) = plugin.next(Some(TaskResult(Map::new()))) else {
                panic!("the module's result ends the item");
            };
            assert_eq!(result.0.get("ansible_facts").cloned(), facts, "{delegated}");
        }
    }
}
