// SPDX-License-Identifier: GPL-3.0-or-later
//! `systemd`, answered in the agent. Not written yet: every task goes to the Python module.

use super::{Native, NativeRun};

pub const NATIVE: Native = Native {
    name: "systemd",
    aliases: &["systemd_service"],
    enabled: false,
    run: |_, _, _| NativeRun::Fallback("not implemented".into()),
};
