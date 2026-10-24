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
#[must_use]
pub fn doc(args: DocArgs) -> i32 {
    crate::runtime::tool::run_tool("doc", &args.args)
}
