// SPDX-License-Identifier: GPL-3.0-or-later
//! `group`, answered in the agent: ansible-core 2.19.12's `group.py`, its `Linux` class, for
//! `name`, `state`, `gid` and `system`. It reads `/etc/group`, runs the module's `groupadd`,
//! `groupmod` or `groupdel` with the module's arguments in the module's order, and answers with
//! the module's keys, `stdout` and `stderr` included when the command printed them.
//!
//! Handed to the Python module before anything changes: any other argument (`local`,
//! `non_unique`, `force`, `gid_min`, `gid_max`), a value the module would convert or refuse, a
//! host outside Debian and Ubuntu (the module takes another class on Alpine), a missing command,
//! and a group the name service knows while `/etc/group` does not hold it (LDAP, sssd).
//!
//! The helpers `user` shares live here too: the host gate, the commands, the account files.

use super::{Native, NativeRun};

pub const NATIVE: Native = Native {
    name: "group",
    aliases: &[],
    enabled: cfg!(unix),
    run,
};

#[cfg(not(unix))]
mod linux {
    use serde_json::{Map, Value};

    use super::NativeRun;
    use crate::modules::Context;

    pub fn run(_: &Map<String, Value>, _: &Context, _: &dyn Fn() -> bool) -> NativeRun {
        NativeRun::Fallback("the native group changes a Linux host".into())
    }
}

pub(super) use linux::*;

#[cfg(unix)]
mod linux;
