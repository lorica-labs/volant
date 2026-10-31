// SPDX-License-Identifier: GPL-3.0-or-later
//! The `host_list` inventory plugin, `-i 'a,b:2222,[::1]:22,'`, and `advanced_host_list`, which
//! also expands ranges: `-i 'web[1:3],'`.

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
            let parsed = if self.advanced {
                expand_hostpattern(item)
            } else {
                ranges::parse_address(item, false).map(|(host, port)| (vec![host], port))
            };
            // What Python raises rather than ansible-core, `int('j')` of a botched range, fails
            // the whole source, measured on 2.19.12; ansible-core's own refusals leave the item
            // as it is written.
            if let Err(reason) = &parsed
                && (reason.starts_with("invalid literal for int()")
                    || reason.contains("values to unpack"))
            {
                return Err(PluginError {
                    message: format!("Invalid data from string, could not parse: {reason}"),
                    origin: Some(format!(
                        "<inventory plugin '{}' with source '{}'>",
                        self.name(),
                        source.text()
                    )),
                    cause: None,
                });
            }
            let (hosts, port) = parsed.unwrap_or_else(|reason| {
                cx.verbose(
                    3,
                    format!("Unable to parse address from hostname, leaving unchanged: {reason}"),
                );
                (vec![item.to_string()], None)
            });
            for host in hosts {
                if into.add_source_host(&host, port, serde_json::Map::new(), None) {
                    into.add_member(&host, "ungrouped");
                }
            }
        }
        Ok(())
    }
}
