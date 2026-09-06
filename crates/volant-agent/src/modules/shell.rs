// SPDX-License-Identifier: GPL-3.0-or-later
//! `shell`: `command` through `sh -c`, so pipes, redirections and variables work.

use std::time::Duration;

use serde_json::{Map, Value};
use volant_protocol::modules::SHELL;

use super::{Module, Run, command};

pub const MODULE: Module = Module {
    spec: &SHELL,
    run: run_shell,
};

fn run_shell(
    args: &Map<String, Value>,
    timeout: Option<Duration>,
    cancelled: &dyn Fn() -> bool,
) -> Run {
    command::execute(args, true, timeout, cancelled)
}
