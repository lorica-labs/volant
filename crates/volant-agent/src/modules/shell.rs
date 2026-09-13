// SPDX-License-Identifier: GPL-3.0-or-later
//! `shell`: `command` through `sh -c`, so pipes, redirections and variables work.

use serde_json::{Map, Value};
use volant_protocol::modules::SHELL;

use super::{Context, Module, Run, command};

pub const MODULE: Module = Module {
    spec: &SHELL,
    run: run_shell,
};

fn run_shell(args: &Map<String, Value>, ctx: &Context, cancelled: &dyn Fn() -> bool) -> Run {
    command::execute(args, true, ctx, cancelled)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    /// The task's `environment` reaches the program the module starts, and a task that set
    /// nothing leaves the name unset.
    ///
    /// What would make this red: `command.envs(&ctx.environment)` dropped from `execute`, which
    /// is how an `environment:` the controller resolved and sent would be accepted and ignored.
    #[test]
    fn the_environment_reaches_the_program() {
        let args: Map<String, Value> = json!({"_raw_params": "echo $VOLANT_X"})
            .as_object()
            .unwrap()
            .clone();
        let mut environment = std::collections::BTreeMap::new();
        environment.insert("VOLANT_X".to_string(), "1".to_string());
        let ctx = Context {
            environment,
            ..Context::default()
        };
        let Run::Done(r) = run_shell(&args, &ctx, &|| false) else {
            panic!("cancelled")
        };
        assert_eq!(r.0["stdout"], "1");
        let Run::Done(r) = run_shell(&args, &Context::default(), &|| false) else {
            panic!("cancelled")
        };
        assert_eq!(r.0["stdout"], "");
    }
}
