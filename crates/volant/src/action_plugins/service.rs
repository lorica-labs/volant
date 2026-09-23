// SPDX-License-Identifier: GPL-3.0-or-later
//! `service`: the host's init system names the module that runs.
//!
//! Read off `plugins/action/service.py` of ansible-core 2.19.12 and measured against it. The
//! init system is `use:` in lower case unless it is `auto`, then `ansible_facts.service_mgr` of
//! the host the module runs on, then a `setup` filtered to that one fact. A name no module of
//! the union carries falls back to `service` rather than failing (measured with `use: nosuchmgr`,
//! which runs `ansible.legacy.service`), and so does a `setup`
//! that failed, as in the reference, which does not look at that result either.
//!
//! As with `package`, the host's answer is only ever looked up in a closed list.

use serde_json::{Map, Value};
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use super::{Context, Kind, Plugin, Step, Sub, fact, gathered, lost, modules_for, setup_for};

/// What `systemd` does not take and the other init modules do, each dropped with a warning:
/// `UNUSED_PARAMS` in the reference, in its order.
const UNUSED_BY_SYSTEMD: [&str; 5] = ["pattern", "runlevel", "sleep", "arguments", "args"];

enum State {
    Start,
    Setup,
    Module(&'static str),
}

pub(super) struct Service<'a> {
    args: &'a Map<String, Value>,
    running_vars: &'a Map<String, Value>,
    warnings: &'a mut Vec<String>,
    state: State,
}

impl<'a> Service<'a> {
    pub(super) fn new(ctx: Context<'a>) -> Self {
        Service {
            args: ctx.args,
            running_vars: ctx.running_vars,
            warnings: ctx.warnings,
            state: State::Start,
        }
    }

    fn dispatch(&mut self, name: &str) -> Step {
        let module = modules_for(Kind::Service)
            .iter()
            .copied()
            .find(|m| *m != "setup" && *m == short_name(name))
            .unwrap_or("service");
        let mut args = self.args.clone();
        args.remove("use");
        // The reference strips by the name as the host or the playbook gave it, so a spelled-out
        // `ansible.builtin.systemd` keeps its arguments there and here.
        if name == "systemd" {
            for unused in UNUSED_BY_SYSTEMD {
                if args.remove(unused).is_some() {
                    // A name from the list above, never a value, so `no_log` has nothing to hide.
                    self.warnings.push(format!(
                        "Ignoring \"{unused}\" as it is not used in \"systemd\""
                    ));
                }
            }
        }
        self.state = State::Module(module);
        Sub::run(module, args)
    }
}

impl Plugin for Service<'_> {
    fn next(&mut self, last: Option<TaskResult>) -> Step {
        match self.state {
            State::Start => {
                let asked = self
                    .args
                    .get("use")
                    .and_then(Value::as_str)
                    .map_or_else(|| "auto".to_string(), str::to_lowercase);
                let chosen = if asked == "auto" {
                    fact(self.running_vars, "service_mgr")
                        .filter(|m| *m != "auto")
                        .map(str::to_string)
                } else {
                    Some(asked)
                };
                if let Some(name) = chosen {
                    self.dispatch(&name)
                } else {
                    self.state = State::Setup;
                    setup_for("ansible_service_mgr")
                }
            }
            State::Setup => {
                let name = last
                    .as_ref()
                    .and_then(|facts| gathered(facts, "ansible_service_mgr"))
                    .unwrap_or("auto")
                    .to_string();
                self.dispatch(&name)
            }
            State::Module(module) => Step::Done(last.unwrap_or_else(|| lost(module))),
        }
    }
}
