// SPDX-License-Identifier: GPL-3.0-or-later
//! What every native shares with the reference's module machinery, `AnsibleModule` of
//! ansible-core 2.19.12 (`module_utils/basic.py`) for the most part.
//!
//! The rule every function here serves: a native answers only where it can say what the
//! reference would say. Where that cannot be known without the Python module, a function
//! returns the reason as an `Err(String)` (or `Stop::HandBack`), and the native hands the task
//! back with it, before anything on the host changed. Whatever runs a program or can take long
//! does so under a `Clock`, the task's `timeout` and the controller's cancel.

use std::path::Path;

use serde_json::{Map, Value};

use super::runtime::{Clock, Stop};

mod accounts;
mod args;
mod backup;
mod exec;
mod fsattrs;
mod mode;

pub(crate) use accounts::files_first;
pub use accounts::{Account, Group, group_account, lookup_group, lookup_user, owner_account};
pub use args::{
    ArgSpec, bool_param, check_names, flag, invocation, module_args, null, path_param, str_param,
};
#[cfg(all(test, target_os = "linux"))]
pub use backup::backup_copy;
pub use backup::{BackupError, backup_local, chown_if_permitted};
#[cfg(test)]
use exec::heuristic_log_sanitize;
pub use exec::{
    bin_path, bytes_repr, clean_args, popen_error, shlex_quote, strerror, validate_runs_as_python,
};
pub use fsattrs::{
    APPEND, AUTOMATIC, FsError, IMMUTABLE, access, add_path_info, failure, has_flags, has_xattrs,
    os_error, parent, realpath, set_fs_attributes, set_fs_attributes_diff,
};
pub use mode::{ModeError, parse_mode, umask};

/// `Stop::TimedOut` once the task's deadline has passed, `Stop::Cancelled` once the controller
/// has cancelled it: for a loop of the native's own that can run long.
pub fn check(clock: Clock) -> Result<(), Stop> {
    if clock
        .deadline
        .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    {
        return Err(Stop::TimedOut);
    }
    if (clock.cancelled)() {
        return Err(Stop::Cancelled);
    }
    Ok(())
}

/// What a native's answer becomes: the result, the hand-back, or, when the task's `timeout`
/// ran out, what the Python path answers when the module outlives it.
pub fn native_run(
    answer: Result<Map<String, Value>, Stop>,
    context: &crate::modules::Context,
) -> super::NativeRun {
    use super::NativeRun;
    match answer {
        Ok(result) => NativeRun::Done(volant_protocol::TaskResult(result)),
        Err(Stop::HandBack(reason)) => NativeRun::Fallback(reason),
        Err(Stop::TimedOut) => NativeRun::Done(volant_protocol::TaskResult::timed_out(
            context.timeout.unwrap_or_default().as_secs(),
        )),
        Err(Stop::Cancelled) => NativeRun::Cancelled,
    }
}

/// The clock of a task: its `timeout` from now, and the controller's cancel.
pub fn clock<'a>(context: &crate::modules::Context, cancelled: &'a dyn Fn() -> bool) -> Clock<'a> {
    Clock {
        deadline: context
            .timeout
            .map(|timeout| std::time::Instant::now() + timeout),
        cancelled,
    }
}

/// Whether SELinux is on, which puts a security context into the reference's results and file
/// operations. A native hands the task back when it is.
pub fn selinux_enabled() -> bool {
    Path::new("/sys/fs/selinux/enforce").exists()
}

#[cfg(test)]
mod tests;

/// What the natives' tests compare against: the recordings of ansible-core 2.19.12 under
/// `crates/volant/tests/golden/module/`, taken on a scratch directory like the one the golden
/// test builds.
#[cfg(all(test, target_os = "linux"))]
pub mod golden;
