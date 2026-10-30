// SPDX-License-Identifier: GPL-3.0-or-later
//! The connection keywords of a play, a block or a task.

/// `remote_user`, `port`, `connection` and the escalation program's keywords `become_method`,
/// `become_exe` and `become_flags`, wherever they are written, as written: each may be a
/// template, rendered when the task runs, except `become_method`, which names one of
/// [`super::BECOME_METHODS`]. A task's own value wins over its block's and its play's, and a host
/// variable wins over all of them; see `Transport::for_task` and `become_for`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connect {
    pub remote_user: Option<String>,
    pub port: Option<String>,
    pub connection: Option<String>,
    pub become_method: Option<String>,
    pub become_exe: Option<String>,
    pub become_flags: Option<String>,
}

impl Connect {
    /// This layer over the one outside it.
    pub fn over(&self, outer: &Connect) -> Connect {
        let pick = |inner: &Option<String>, outer: &Option<String>| {
            inner.clone().or_else(|| outer.clone())
        };
        Connect {
            remote_user: pick(&self.remote_user, &outer.remote_user),
            port: pick(&self.port, &outer.port),
            connection: pick(&self.connection, &outer.connection),
            become_method: pick(&self.become_method, &outer.become_method),
            become_exe: pick(&self.become_exe, &outer.become_exe),
            become_flags: pick(&self.become_flags, &outer.become_flags),
        }
    }
}
