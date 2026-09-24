// SPDX-License-Identifier: GPL-3.0-or-later
//! `user`, answered in the agent. Not written yet: every task goes to the Python module.

use super::{Native, NativeRun};

pub const NATIVE: Native = Native {
    name: "user",
    aliases: &[],
    enabled: false,
    run: |_, _, _| NativeRun::Fallback("not implemented".into()),
};
