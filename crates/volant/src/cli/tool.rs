// SPDX-License-Identifier: GPL-3.0-or-later
//! The `doc`, `config`, `galaxy` and `console` commands: the ansible-core program of the same name,
//! run from the managed runtime.

use std::ffi::OsString;

use clap::Args;

/// Every argument goes to the program unchanged, `-h` and `--version` included.
#[derive(Args, Debug)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct ToolArgs {
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    pub(super) args: Vec<OsString>,
}

/// Runs `ansible-doc` and returns its exit code.
#[must_use]
pub fn doc(args: ToolArgs) -> i32 {
    through("doc", args)
}

/// Runs `ansible-config` and returns its exit code.
#[must_use]
pub fn config(args: ToolArgs) -> i32 {
    through("config", args)
}

/// Runs `ansible-galaxy` and returns its exit code.
#[must_use]
pub fn galaxy(args: ToolArgs) -> i32 {
    through("galaxy", args)
}

/// Runs `ansible-console` and returns its exit code.
#[must_use]
pub fn console(args: ToolArgs) -> i32 {
    through("console", args)
}

/// clap drops a leading `--`, which the tools read as the end of their options, so the
/// arguments are taken again from the command line: everything after the subcommand, or after
/// the program name when started as `ansible-<tool>`.
fn through(tool: &str, args: ToolArgs) -> i32 {
    let mut raw = std::env::args_os();
    let started = raw.next().unwrap_or_default();
    if super::subcommand_for(&started).is_none() {
        raw.next();
    }
    let raw: Vec<OsString> = raw.collect();
    let tail = if raw.len() >= args.args.len() {
        raw
    } else {
        args.args
    };
    crate::runtime::tool::run_tool(tool, &tail)
}
