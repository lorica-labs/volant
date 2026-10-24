// SPDX-License-Identifier: GPL-3.0-or-later
//! The `doc` command: `ansible-doc`, run from the managed runtime.

use std::ffi::OsString;

use clap::Args;

/// Every argument goes to `ansible-doc` unchanged, `-h` and `--version` included.
#[derive(Args, Debug)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct DocArgs {
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    pub(super) args: Vec<OsString>,
}

/// Runs `ansible-doc` and returns its exit code.
///
/// clap drops a leading `--`, which `ansible-doc` reads as the end of its options, so the
/// arguments are taken again from the command line: everything after `doc`, or after the
/// program name when started as `ansible-doc`.
#[must_use]
pub fn doc(args: DocArgs) -> i32 {
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
    crate::runtime::tool::run_tool("doc", &tail)
}
