// SPDX-License-Identifier: GPL-3.0-or-later
//! The `host_list` inventory plugin: `-i 'a,b:2222,[::1]:22,'`.

use std::collections::BTreeMap;

use super::{Inventory, InventoryPlugin, ParseCx, PluginError, Source, ranges};

pub(super) struct HostList;

impl InventoryPlugin for HostList {
    fn name(&self) -> &'static str {
        "host_list"
    }

    /// A comma, and no file of that name.
    fn verify(&self, source: &Source) -> bool {
        source.text().contains(',') && !source.path().exists()
    }

    /// Each non-empty item a host of `ungrouped`, with the port it names; an item that is no
    /// address is a host name as written. A host an earlier source has stays as it is.
    fn parse(
        &self,
        source: &Source,
        into: &mut Inventory,
        cx: &ParseCx,
    ) -> Result<(), PluginError> {
        for item in source.text().split(',').map(str::trim) {
            if item.is_empty() {
                continue;
            }
            let (host, port) = ranges::parse_address(item, false).unwrap_or_else(|reason| {
                cx.verbose(
                    3,
                    format!("Unable to parse address from hostname, leaving unchanged: {reason}"),
                );
                (item.to_string(), None)
            });
            if into.add_source_host(&host, port, BTreeMap::new(), None) {
                into.add_member(&host, "ungrouped");
            }
        }
        Ok(())
    }
}
