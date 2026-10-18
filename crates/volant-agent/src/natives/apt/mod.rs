// SPDX-License-Identifier: GPL-3.0-or-later
//! `apt`, answered in the agent when the task asks for what the host already has: a package
//! cache that is still fresh, packages that are all installed, or packages none of which is.
//! Every other task goes to the Python module, handed back before anything on the host changed.
//!
//! Read from ansible-core 2.19.12's `apt.py`, whose answers in those three cases are:
//!
//! - no package, and a fresh cache or no cache check at all:
//!   `{"changed": false, "cache_updated": false, "cache_update_time": T}` (`{"changed": false}`
//!   for `state: absent` without a check);
//! - `state: present`, every package `install ok installed` and marked manually installed: the
//!   same three keys;
//! - `state: absent`, no package installed: `{"changed": false}`.
//!
//! One divergence: for `state: present` the module runs `apt-mark manual <packages>` whether or
//! not anything changes. With every name already manual that call is a no-op, and the native
//! does not spawn it: an `apt-mark` that would fail there without changing anything is not
//! reported. A name marked automatically installed, where the call changes something, goes to
//! the module.
//!
//! The native reads files only (`/var/lib/dpkg/status`, `/var/lib/apt/extended_states`, the
//! cache stamp) and takes no lock. dpkg replaces its status file by renaming a complete new one
//! over it, so a read sees one version or the other, never a mix; a journal left in
//! `/var/lib/dpkg/updates/` means the status file is not the whole story, and hands back.

use std::time::SystemTime;

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use super::runtime::Root;
use super::{Native, NativeRun};
use crate::modules::Context;

mod plan;
mod request;
mod spec;
mod status;
#[cfg(all(test, target_os = "linux"))]
pub(super) mod tests;

use plan::plan;

pub const NATIVE: Native = Native {
    name: "apt",
    aliases: &[],
    enabled: true,
    run,
};

pub(super) const STATUS: &str = "/var/lib/dpkg/status";
pub(super) const UPDATES: &str = "/var/lib/dpkg/updates";
pub(super) const EXTENDED_STATES: &str = "/var/lib/apt/extended_states";
pub(super) const STAMP: &str = "/var/lib/apt/periodic/update-success-stamp";
pub(super) const LISTS: &str = "/var/lib/apt/lists";

/// Reads files only and spawns nothing, so there is no `timeout` or cancel to honour.
fn run(args: &Map<String, Value>, context: &Context, _: &dyn Fn() -> bool) -> NativeRun {
    answer(args, &Root::real(), context, SystemTime::now())
}

fn answer(args: &Map<String, Value>, root: &Root, context: &Context, now: SystemTime) -> NativeRun {
    match plan(args, root, &context.environment, now) {
        Err(reason) => NativeRun::Fallback(reason),
        Ok(result) => NativeRun::Done(TaskResult(result)),
    }
}
