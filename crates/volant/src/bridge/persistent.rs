// SPDX-License-Identifier: GPL-3.0-or-later
//! The persistent connection processes ansible-core starts for `network_cli`, `netconf` and
//! `httpapi` hosts: the socket each host's tasks use, kept as the strategy's
//! `_active_connections` keeps it, and their end.

use serde_json::json;

use super::{Bridge, PluginKind};

impl Bridge {
    /// Keeps the socket a task of `host` used; the first one stays, as ansible-core keeps it.
    pub(super) fn note_socket(&self, host: &str, path: &str) {
        self.lock_sockets()
            .entry(host.to_string())
            .or_insert_with(|| path.to_string());
    }

    /// Ends the persistent processes of `host`, or of every host with `None`, as
    /// `meta: reset_connection` and the end of a play end them. A process already gone, or a
    /// helper that cannot be asked, is no failure: ansible-core only logs it at debug level, and
    /// the process ends anyway after `persistent_connect_timeout` idle.
    pub fn reset_persistent(&self, host: Option<&str>) {
        let paths: Vec<String> = {
            let mut sockets = self.lock_sockets();
            match host {
                Some(host) => sockets.remove(host).into_iter().collect(),
                None => std::mem::take(&mut *sockets).into_values().collect(),
            }
        };
        if paths.is_empty() {
            return;
        }
        let call = json!({
            "kind": "reset_persistent", "name": "reset_persistent", "args": paths, "kwargs": {},
        });
        _ = self.request(PluginKind::Lookup, "reset_persistent", call);
    }

    /// The sockets kept, by host.
    pub fn persistent_sockets(&self) -> Vec<(String, String)> {
        let mut sockets: Vec<_> = self.lock_sockets().clone().into_iter().collect();
        sockets.sort();
        sockets
    }

    fn lock_sockets(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, String>> {
        self.sockets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
