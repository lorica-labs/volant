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
mod tests;
