// SPDX-License-Identifier: GPL-3.0-or-later
//! The `host_list` inventory plugin, `-i 'a,b:2222,[::1]:22,'`, and `advanced_host_list`, which
//! also expands ranges: `-i 'web[1:3],'`.

use std::collections::BTreeMap;

use super::ini::expand_hostpattern;
use super::{Inventory, InventoryPlugin, ParseCx, PluginError, Source, ranges};

pub(super) struct HostList {
    /// Read ranges too, which is what `advanced_host_list` adds.
    pub(super) advanced: bool,
}

impl InventoryPlugin for HostList {
    fn name(&self) -> &'static str {
        if self.advanced {
            "advanced_host_list"
        } else {
            "host_list"
        }
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
            let (hosts, port) = if self.advanced {
                expand_hostpattern(item)
            } else {
                ranges::parse_address(item, false).map(|(host, port)| (vec![host], port))
            }
            .unwrap_or_else(|reason| {
                cx.verbose(
                    3,
                    format!("Unable to parse address from hostname, leaving unchanged: {reason}"),
                );
                (vec![item.to_string()], None)
            });
            for host in hosts {
                if into.add_source_host(&host, port, BTreeMap::new(), None) {
                    into.add_member(&host, "ungrouped");
                }
            }
        }
        Ok(())
    }
}
