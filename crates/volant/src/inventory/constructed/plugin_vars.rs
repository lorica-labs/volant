// SPDX-License-Identifier: GPL-3.0-or-later
//! What `use_vars_plugins` adds to the variables a host's expressions see: what the vars plugins
//! answer at the stage `all` on the directory of each source read before, as
//! `get_vars_from_inventory_sources` runs them while the inventory is still being read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::inventory::{Inventory, ParseCx};
use crate::tags::Tagged;

/// Per name, what one plugin answers for a group, or for a host.
type Answers = BTreeMap<String, Tagged>;

#[derive(Default)]
pub(super) struct PluginVars {
    /// Per directory, in source order, then per plugin in the loader's order, `host_group_vars`
    /// among them: what it answers for each group, then for each host.
    dirs: Vec<Vec<(Answers, Answers)>>,
}

impl PluginVars {
    /// Measured on ansible-core 2.19.12: the directories are those of the sources read before,
    /// not the plugin's own. `host_group_vars` reads its files there when it runs at the stage
    /// `all`, and every other plugin the Python helper loads answers there.
    pub(super) fn read(inv: &Inventory, cx: &ParseCx) -> anyhow::Result<Self> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for path in inv.hosts.iter().filter_map(|h| inv.source_of(&h.name)) {
            let root = if path.is_dir() {
                Some(path)
            } else {
                path.parent()
            };
            if let Some(root) = root.map(Path::to_path_buf)
                && !roots.contains(&root)
            {
                roots.push(root);
            }
        }
        let entities: Vec<(bool, String)> = inv
            .groups
            .iter()
            .map(|g| (true, g.name.clone()))
            .chain(inv.hosts.iter().map(|h| (false, h.name.clone())))
            .collect();
        let (after, answers) = if cx.config.other_vars_plugins && !roots.is_empty() {
            crate::vars::answers_at_all(Arc::clone(cx.helper()), inv, &roots, &entities)?
        } else {
            (Vec::new(), Vec::new())
        };
        let before = after.iter().filter(|after| !**after).count();
        let mut dirs = Vec::new();
        for (at, root) in roots.iter().enumerate() {
            let mut layers: Vec<(Answers, Answers)> = std::iter::repeat_with(Default::default)
                .take(after.len() + 1)
                .collect();
            if cx.config.host_group_vars_at_all {
                layers[before] = (
                    crate::vars::load_vars_dir(&root.join("group_vars"))?,
                    crate::vars::load_vars_dir(&root.join("host_vars"))?,
                );
            }
            for (entity, index, vars) in answers.get(at).into_iter().flatten() {
                let (group, name) = &entities[*entity];
                let layer = &mut layers[index + usize::from(*index >= before)];
                let answers = if *group { &mut layer.0 } else { &mut layer.1 };
                answers.entry(name.clone()).or_default().extend(vars);
            }
            dirs.push(layers);
        }
        Ok(Self { dirs })
    }

    /// `vars`, a host's inventory variables, with the plugins' answers under them: the reference
    /// lays what they answer for the host's groups over the groups' variables, directory by
    /// directory and plugin by plugin, then what they answer for the host over its own.
    pub(super) fn over(&self, inv: &Inventory, host: &str, mut vars: Tagged) -> Tagged {
        if self.dirs.is_empty() {
            return vars;
        }
        let mut groups = Vec::new();
        if inv.placed_in_all(host) {
            groups.push("all".to_string());
        }
        groups.extend(inv.layered_groups_of(host));
        for (answers, _) in self.dirs.iter().flatten() {
            for group in &groups {
                if let Some(found) = answers.get(group) {
                    vars.extend(found);
                }
            }
        }
        if let Some(own) = inv.host_line_vars(host) {
            vars.extend(own);
        }
        for (_, answers) in self.dirs.iter().flatten() {
            if let Some(found) = answers.get(host) {
                vars.extend(found);
            }
        }
        vars
    }
}
