// SPDX-License-Identifier: GPL-3.0-or-later
//! The request that runs a run's vars plugins in the helper.

use serde_json::{Value, json};

use super::{Bridge, BridgeError, PluginKind};

impl Bridge {
    /// What the vars plugins other than `host_group_vars` answer for the calls `args` holds, and
    /// the plugins in the order the loader runs them.
    ///
    /// # Errors
    /// [`BridgeError`] when the helper cannot start, or a plugin raised.
    pub fn vars_plugins(&self, args: &Value) -> Result<Value, BridgeError> {
        self.request(
            PluginKind::Lookup,
            "vars_plugins",
            json!({"kind": "vars_plugins", "name": "vars_plugins", "args": [args], "kwargs": {}}),
        )
    }
}
