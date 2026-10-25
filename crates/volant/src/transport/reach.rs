// SPDX-License-Identifier: GPL-3.0-or-later
//! What putting the agent on a target needs of the local command that reaches it: the command
//! line that runs one shell command there, the environment it starts with, the `-vvvv` lines, and
//! how a failure of that command is told from the remote command's own status. `ssh` and every
//! exec-with-stdio command (`docker exec -i` and the like) answer it, so the probe, the upload
//! and the start of the agent are written once.

use std::ffi::OsString;

use super::bootstrap::Captured;
use super::defaults::{ConnectError, SshTarget};

pub(super) trait Reach: Sync {
    /// The target's name in messages and in the `<...>` of the `-vvvv` lines.
    fn place(&self) -> &str;

    /// Where the agent's directory goes, as the playbook wrote it.
    fn remote_tmp(&self) -> &str;

    /// The local command line that runs `command` with a POSIX shell in the target, with stdin
    /// piped when `payload` is set.
    fn command_line(&self, command: &str, payload: bool) -> Vec<String>;

    /// The environment the local command starts with.
    ///
    /// # Errors
    /// Why it could not be made, such as a password that could not be stored.
    fn environment(&self) -> Result<Vec<(&'static str, OsString)>, ConnectError>;

    /// The `-vvvv` line of a command about to run.
    fn trace_exec(&self, argv: &[String]);

    /// The `-vvvv` line of a command that ended.
    fn trace_result(&self, captured: &Captured);

    /// The failure of the reaching command itself that `captured` shows, as opposed to the
    /// remote command's own status: `ssh` reports its own with 255. `offered` is the `sudo`
    /// password that went on stdin, if one did, so no message can repeat it.
    fn connection_failure(
        &self,
        captured: &Captured,
        offered: Option<&str>,
    ) -> Option<ConnectError>;

    /// What the first probe's status says when it is none of the bootstrap's own codes: `None`
    /// when the machine is asked for its architecture again, separately.
    fn probe_failure(&self, _probe: &Captured) -> Option<ConnectError> {
        None
    }

    /// The `ssh` target behind this one, for what only `ssh` does: `su`, which needs a terminal.
    fn over_ssh(&self) -> Option<&SshTarget> {
        None
    }
}
