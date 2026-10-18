// SPDX-License-Identifier: GPL-3.0-or-later
//! `user`, answered in the agent: ansible-core 2.19.12's `user.py`, its generic `User` class that
//! Debian and Ubuntu get, for `name`, `state`, `uid`, `group`, `groups`, `append`, `comment`,
//! `home`, `shell`, `system` and `create_home`. It reads `/etc/passwd` and `/etc/group`, runs
//! the module's `useradd`, `usermod` or `userdel` with the module's arguments in the module's
//! order, and answers with the module's keys, `stdout` and `stderr` included when the command
//! printed them.
//!
//! Handed to the Python module before anything changes: any other argument (`local`,
//! `non_unique`, every `password*`, `expires`, `generate_ssh_key`, every `ssh_key_*`, `remove`,
//! `move_home`, `seuser`, `login_class`, `umask`, `password_lock`, `force` and the rest), a value
//! the module would convert or refuse, `append` without `groups` (the module warns), a group
//! that does not exist (the module fails), a home the module would create itself (a parent that
//! is missing, or a missing home under `create_home`), a host outside Debian and Ubuntu, and an
//! account the name service knows while `/etc/passwd` does not hold it (LDAP, sssd).

use super::{Native, NativeRun};

pub const NATIVE: Native = Native {
    name: "user",
    aliases: &[],
    enabled: cfg!(unix),
    run: linux::run,
};

#[cfg(not(unix))]
mod linux {
    use serde_json::{Map, Value};

    use super::NativeRun;
    use crate::modules::Context;

    pub fn run(_: &Map<String, Value>, _: &Context, _: &dyn Fn() -> bool) -> NativeRun {
        NativeRun::Fallback("the native user changes a Linux host".into())
    }
}

#[cfg(unix)]
mod linux;
