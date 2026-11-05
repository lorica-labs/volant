// SPDX-License-Identifier: GPL-3.0-or-later
//! A module run from an AnsiballZ wrapper on the host, built and copied there by ansible-core's
//! own `normal`, in place of the agent's run of it.

use std::sync::atomic::{AtomicBool, Ordering};

use volant_protocol::modules::short_name;

use super::{Kind, kind};
use crate::playbook::PlayTask;

static KEEP_REMOTE_FILES: AtomicBool = AtomicBool::new(false);

pub(super) const WRAPPER: Kind = Kind::Bridged("ansible.builtin.normal");

/// Sets `keep_remote_files`, once, before the first task.
pub(crate) fn keep_remote_files(on: bool) {
    KEEP_REMOTE_FILES.store(on, Ordering::Relaxed);
}

/// Whether a task with no plugin of its own runs from a wrapper rather than by the agent: what
/// `keep_remote_files` keeps is that wrapper, and a host told not to pipeline asks for it by name.
/// `raw` and `async_status` have none in the reference either, and a job's wrapper is
/// `async_wrapper`, whose status the agent could not read. A task escalating to `unprivileged` user
/// stays with the agent too: the wrapper sits in a directory only the connection user reads.
fn runs_from_wrapper(task: &PlayTask, no_pipelining: bool, unprivileged: bool) -> bool {
    (KEEP_REMOTE_FILES.load(Ordering::Relaxed) || no_pipelining)
        && !unprivileged
        && task.async_val == 0
        && !matches!(short_name(&task.module), "raw" | "async_status")
}

/// The plugin that runs `task`, if a plugin does, on a host whose variables say `no_pipelining`
/// or not, escalating to an `unprivileged` user or not.
pub(crate) fn kind_of(task: &PlayTask, no_pipelining: bool, unprivileged: bool) -> Option<Kind> {
    kind(&task.module)
        .or_else(|| runs_from_wrapper(task, no_pipelining, unprivileged).then_some(WRAPPER))
}

#[cfg(test)]
mod tests {
    use super::{WRAPPER, keep_remote_files, kind_of};
    use crate::action_plugins::Kind;
    use crate::testing::task;

    /// What `keep_remote_files` keeps is the wrapper of a module that has one in the reference;
    /// a plugin this engine runs itself stays that plugin.
    #[test]
    fn keep_remote_files_runs_a_module_from_its_wrapper() {
        let of = |t: &crate::playbook::PlayTask| kind_of(t, false, false);
        assert_eq!(of(&task("ping")), None);
        keep_remote_files(true);
        assert_eq!(of(&task("ping")), Some(WRAPPER));
        assert_eq!(of(&task("ansible.legacy.setup")), Some(WRAPPER));
        assert_eq!(of(&task("raw")), None);
        assert_eq!(of(&task("ansible.builtin.async_status")), None);
        assert_eq!(of(&task("copy")), Some(Kind::Copy));
        let mut job = task("ping");
        job.async_val = 10;
        assert_eq!(of(&job), None);
        keep_remote_files(false);
    }

    #[test]
    fn a_host_told_not_to_pipeline_runs_a_module_from_its_wrapper() {
        assert_eq!(kind_of(&task("ping"), true, false), Some(WRAPPER));
        assert_eq!(kind_of(&task("raw"), true, false), None);
        assert_eq!(kind_of(&task("copy"), true, false), Some(Kind::Copy));
        // Its user could not read the wrapper.
        assert_eq!(kind_of(&task("ping"), true, true), None);
    }
}
