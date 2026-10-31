// SPDX-License-Identifier: GPL-3.0-or-later
//! `meta: refresh_inventory`: what the store holds of the inventory, read again.

use crate::inventory::Inventory;

use super::store::inventory_maps;
use super::{Added, VarStore};

impl VarStore {
    /// Replaces what the store knows of the inventory with `inventory`'s groups, hosts and
    /// variables, as `meta: refresh_inventory` does mid-play, and puts back the hosts and groups
    /// `add_host` and `group_by` made. The facts hosts gathered and the facts the playbook set
    /// stay where they are: the reference's refresh re-reads its sources and touches neither.
    /// Returns the inventory the store now holds.
    pub fn refresh_inventory(&mut self, inventory: &Inventory) -> Inventory {
        self.dynamic = inventory.clone();
        // The hosts `add_host` made first, then the groupings, as `InventoryManager.refresh_inventory`
        // replays them: measured on ansible-core 2.19.12, a grouping of a host the sources no
        // longer hold is dropped, rather than bringing the host back.
        for change in &self.added {
            if let Added::Host(name, vars) = change {
                self.dynamic.add_host(name, vars.clone());
            }
        }
        for change in &self.added {
            match change {
                Added::Member(host, group) if self.dynamic.groups()["all"].contains(host) => {
                    self.dynamic.add_member(host, group);
                }
                Added::Child(parent, child) => self.dynamic.add_child(parent, child),
                _ => {}
            }
        }
        (
            self.groups,
            self.inventory_vars,
            self.host_line_vars,
            self.group_names,
        ) = inventory_maps(&self.dynamic);
        self.mark_inventory_names();
        // The reference keeps `--limit` as patterns, applied anew to what the sources hold.
        if let Some((pattern, hosts)) = &mut self.limit {
            *hosts = self
                .dynamic
                .resolve(pattern)
                .hosts
                .into_iter()
                .map(|h| h.name)
                .collect();
        }
        self.inventory_changed = false;
        self.forget_hostvars();
        self.shared = None;
        self.dynamic.clone()
    }
}
