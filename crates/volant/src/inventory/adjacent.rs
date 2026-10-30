// SPDX-License-Identifier: GPL-3.0-or-later
//! An enabled inventory plugin no builtin answers to: one in an `inventory_plugins/` beside the
//! playbook or a role, or a collection's, which the controller's ansible-core loads and runs.

use super::{Inventory, InventoryPlugin, ParseCx, PluginError, Source};

/// What [`Adjacent::parse`] answers for a source the plugin's `verify_file` refused: the chain
/// reads it as a source the plugin declined, not as a failure.
pub(super) const DECLINED: &str = "\u{0}declined";

/// The plugin `name`, run in the Python helper.
pub(super) struct Adjacent(pub(super) &'static str);

impl InventoryPlugin for Adjacent {
    fn name(&self) -> &'static str {
        self.0
    }

    // Asked of the plugin itself, in the helper, when the source is parsed.
    fn verify(&self, _source: &Source) -> bool {
        true
    }

    fn parse(
        &self,
        source: &Source,
        into: &mut Inventory,
        cx: &ParseCx,
    ) -> Result<(), PluginError> {
        let answer = cx
            .helper()
            .inventory_plugin(self.0, source.text())
            .map_err(|e| super::auto::failed(e.to_string()))?;
        if answer.get("declined").and_then(serde_json::Value::as_bool) == Some(true) {
            return Err(super::auto::failed(DECLINED.to_string()));
        }
        super::collection::read(&answer, source, into, cx)
    }
}
