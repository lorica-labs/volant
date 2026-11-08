// SPDX-License-Identifier: GPL-3.0-or-later
//! The vars plugins of a run, and the stages they run at. Volant reads `host_group_vars` itself;
//! ansible-core runs every other one in the helper, and what they answer sits where the reference
//! merges it.
//!
//! Every root of the store, an inventory source's directory or the playbook's, holds its
//! `group_vars` and `host_vars` maps between those of the plugins the loader runs before
//! `host_group_vars` and those it runs after, one map per plugin. The store's loops over the
//! roots then merge a path's plugins in the loader's order, as `get_vars_from_path` does. What
//! the plugins answer at the inventory stage is laid over the inventory's own variables instead.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::load::load_vars_dir;
use super::store::inventory_maps;
use super::{Tagged, VarStore};
use crate::bridge::{Bridge, BridgeError};
use crate::inventory::{Inventory, LOCALHOST_NAMES, Source};
use crate::stats::Refusal;
use crate::tags::OriginId;

/// A group (`true`) or a host, by name.
type Entity = (bool, String);

/// Per call, what each plugin answered for each entity: the entity's place in the call, the
/// plugin's place, and its variables.
type Answers = Vec<Vec<(usize, usize, Map<String, Value>)>>;

/// What one root's `group_vars` and `host_vars` hold.
type Files = (BTreeMap<String, Tagged>, BTreeMap<String, Tagged>);

/// Whether `host_group_vars` runs at the inventory stage, and at the store's own stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostGroupVars {
    pub inventory: bool,
    pub task: bool,
}

/// What the store keeps of its vars plugins.
#[derive(Debug)]
pub(super) struct Staging {
    /// The inventory sources' directories, where the inventory stage looks.
    inventory_roots: Vec<PathBuf>,
    host_group_vars: HostGroupVars,
    /// The plugins the helper runs, when it loads any.
    run: Option<VarsPluginRun>,
}

#[derive(Debug)]
struct VarsPluginRun {
    bridge: Arc<Bridge>,
    /// The stage the store's own lookups run at: `task` for a playbook, `all` for an inventory
    /// listing, as `ansible-inventory` asks.
    stage: &'static str,
    /// Each plugin's origin, and whether the loader runs it after `host_group_vars`.
    plugins: Vec<(OriginId, bool)>,
    /// The groups and hosts already asked about at `stage`.
    asked: BTreeSet<Entity>,
}

impl VarsPluginRun {
    /// How many plugins run before `host_group_vars`, and how many after.
    fn split(&self) -> (usize, usize) {
        let after = self.plugins.iter().filter(|(_, after)| *after).count();
        (self.plugins.len() - after, after)
    }

    /// One request to the helper. Each call is a stage, a path, and the entities to ask about.
    fn ask(
        &mut self,
        inventory: &Inventory,
        basedir: &Path,
        calls: &[(&str, &Path, &[Entity])],
    ) -> anyhow::Result<Answers> {
        let calls: Vec<Value> = calls
            .iter()
            .map(|(stage, path, entities)| {
                let entities: Vec<Value> = entities
                    .iter()
                    .map(|(group, name)| json!([if *group { "group" } else { "host" }, name]))
                    .collect();
                json!({"stage": stage, "path": path.to_string_lossy(), "entities": entities})
            })
            .collect();
        let answer = self.bridge.vars_plugins(&json!({
            "dirs": [],
            "basedir": basedir.to_string_lossy(),
            "inventory": inventory.plugin_view(),
            "calls": calls,
        }))?;
        // Measured on ansible-core 2.19.12: an `AnsibleError` a plugin raises stops the run with
        // its message and exit code 1, any other exception with the line below and 250.
        if let Some(failed) = answer["failed"].as_str() {
            return Err(if answer["unexpected"] == true {
                Refusal::at(
                    250,
                    format!("Unexpected Exception, this is probably a bug: {failed}"),
                )
            } else {
                Refusal::at(1, failed)
            });
        }
        let shape =
            || BridgeError::Helper("the vars plugins answer has an unexpected shape".into());
        if self.plugins.is_empty() {
            for plugin in answer["plugins"].as_array().ok_or_else(shape)? {
                let name = plugin["name"].as_str().unwrap_or_default();
                let path = plugin["path"].as_str().unwrap_or_default();
                let origin =
                    crate::tags::origin_described(&format!("<vars plugin '{name}' from '{path}'>"));
                self.plugins.push((origin, plugin["after"] == true));
            }
        }
        let mut out = Vec::new();
        for call in answer["answers"].as_array().ok_or_else(shape)? {
            let mut found = Vec::new();
            for entry in call.as_array().ok_or_else(shape)? {
                let (Some(at), Some(index), Some(vars)) =
                    (entry[0].as_u64(), entry[1].as_u64(), entry[2].as_object())
                else {
                    return Err(shape().into());
                };
                let (Ok(at), Ok(index)) = (usize::try_from(at), usize::try_from(index)) else {
                    return Err(shape().into());
                };
                found.push((at, index, vars.clone()));
            }
            out.push(found);
        }
        Ok(out)
    }

    /// What plugin `index` answered, each value with the plugin for its origin and untrusted
    /// where the helper marked it.
    fn tagged(&self, index: usize, vars: &Map<String, Value>) -> Tagged {
        let origin = self.plugins[index].0;
        let mut tagged = Tagged::default();
        for (name, value) in vars {
            let (value, tree) = crate::yaml::unsafe_markers(value, origin);
            tagged.insert(name.clone(), value, tree);
        }
        tagged
    }
}

impl Staging {
    /// How many plugin maps come before a root's own files, and how many after.
    fn layout(&self) -> (usize, usize) {
        self.run.as_ref().map_or((0, 0), VarsPluginRun::split)
    }

    /// The inventory stage, once every source is read, as `InventoryManager.parse_sources` runs
    /// it: what the plugins answer on each inventory root for each group and host of `inventory`
    /// is laid over what the inventory gives it, root by root and plugin by plugin. `files` are
    /// the roots' `group_vars` and `host_vars`, read here when `host_group_vars` runs at this
    /// stage. A plugin left to `demand` does not run.
    fn inventory_stage(
        &mut self,
        inventory: &mut Inventory,
        basedir: &Path,
        files: &[Files],
    ) -> anyhow::Result<()> {
        let groups = inventory.groups();
        let entities: Vec<Entity> = groups
            .keys()
            .map(|g| (true, g.clone()))
            .chain(groups["all"].iter().map(|h| (false, h.clone())))
            .collect();
        let answers = match self.run.as_mut() {
            Some(run) => {
                let calls: Vec<_> = self
                    .inventory_roots
                    .iter()
                    .map(|root| ("inventory", root.as_path(), entities.as_slice()))
                    .collect();
                run.ask(inventory, basedir, &calls)?
            }
            None => Vec::new(),
        };
        let before = self.layout().0;
        for root in 0..self.inventory_roots.len() {
            let answer = answers.get(root).map(Vec::as_slice).unwrap_or_default();
            let run = self.run.as_ref();
            let lay =
                |inventory: &mut Inventory,
                 (at, index, vars): &(usize, usize, Map<String, Value>)| {
                    if let Some(run) = run {
                        let (group, name) = &entities[*at];
                        inventory.merge_vars(*group, name, &run.tagged(*index, vars));
                    }
                };
            for early in answer.iter().filter(|a| a.1 < before) {
                lay(inventory, early);
            }
            if self.host_group_vars.inventory
                && let Some((groups, hosts)) = files.get(root)
            {
                for (name, vars) in groups {
                    inventory.merge_vars(true, name, vars);
                }
                for (name, vars) in hosts {
                    inventory.merge_vars(false, name, vars);
                }
            }
            for late in answer.iter().filter(|a| a.1 >= before) {
                lay(inventory, late);
            }
        }
        Ok(())
    }
}

impl VarStore {
    /// Runs the vars plugins at the inventory stage on the inventory's own variables, then at
    /// `stage` on every group and host: `host_group_vars` when `host_group_vars` says it runs,
    /// and every other plugin the helper of `bridge` loads. A run that keeps `host_group_vars`
    /// to the task stage and loads no other plugin is left as it was.
    ///
    /// # Errors
    /// The [`BridgeError`] of a helper that cannot start, or the [`Refusal`] of a plugin that
    /// raised.
    pub fn load_vars_plugins(
        &mut self,
        sources: &[Source],
        stage: &'static str,
        host_group_vars: HostGroupVars,
        bridge: Option<Arc<Bridge>>,
    ) -> anyhow::Result<()> {
        let mut staging = Staging {
            inventory_roots: sources.iter().filter_map(Source::vars_dir).collect(),
            host_group_vars,
            run: bridge.map(|bridge| VarsPluginRun {
                bridge,
                stage,
                plugins: Vec::new(),
                asked: BTreeSet::new(),
            }),
        };
        let files: Vec<Files> = (0..staging.inventory_roots.len())
            .filter(|_| host_group_vars.inventory)
            .map(|root| {
                (
                    self.group_files[root].clone(),
                    self.host_files[root].clone(),
                )
            })
            .collect();
        staging.inventory_stage(&mut self.dynamic, &self.playbook_dir, &files)?;
        if staging
            .run
            .as_ref()
            .is_some_and(|run| run.plugins.is_empty())
        {
            staging.run = None;
        }
        let plain = HostGroupVars {
            inventory: false,
            task: true,
        };
        if staging.run.is_none() && host_group_vars == plain {
            return Ok(());
        }
        let (before, after) = staging.layout();
        let width = before + 1 + after;
        for files in [&mut self.group_files, &mut self.host_files] {
            *files = std::mem::take(files)
                .into_iter()
                .map(|map| {
                    if host_group_vars.task {
                        map
                    } else {
                        BTreeMap::new()
                    }
                })
                .flat_map(|map| {
                    let empty = || std::iter::repeat_with(BTreeMap::new);
                    empty().take(before).chain([map]).chain(empty().take(after))
                })
                .collect();
        }
        debug_assert_eq!(self.group_files.len() % width, 0);
        self.vars_plugins = Some(Box::new(staging));
        self.reread_inventory();
        self.ask_vars_plugins()
    }

    /// How many plugin maps come before a root's own files and how many after: none without
    /// vars plugins.
    pub(super) fn plugin_layout(&self) -> (usize, usize) {
        self.vars_plugins.as_ref().map_or((0, 0), |s| s.layout())
    }

    /// Whether `host_group_vars` reads the playbook's `group_vars` and `host_vars`.
    pub(super) fn host_group_vars_at_task(&self) -> bool {
        self.vars_plugins
            .as_ref()
            .is_none_or(|s| s.host_group_vars.task)
    }

    /// The inventory as the store holds it now: what the sources read, with what the vars plugins
    /// answered at the inventory stage and what `add_host` and `group_by` changed.
    pub fn inventory(&self) -> &Inventory {
        &self.dynamic
    }

    /// The store's maps of the inventory, built again from `dynamic`.
    fn reread_inventory(&mut self) {
        (
            self.groups,
            self.inventory_vars,
            self.host_line_vars,
            self.group_names,
        ) = inventory_maps(&self.dynamic);
        self.forget_hostvars();
        self.shared = None;
    }

    /// Asks the plugins about the groups and hosts the store holds that they were not asked
    /// about yet, the implicit controller names included, on every root.
    pub(super) fn ask_vars_plugins(&mut self) -> anyhow::Result<()> {
        let Some(staging) = self.vars_plugins.as_ref() else {
            return Ok(());
        };
        let Some(run) = &staging.run else {
            return Ok(());
        };
        let hosts = self.groups.get("all").into_iter().flatten().cloned();
        let entities: BTreeSet<Entity> = self
            .groups
            .keys()
            .map(|g| (true, g.clone()))
            .chain(
                hosts
                    .chain(LOCALHOST_NAMES.map(String::from))
                    .map(|h| (false, h)),
            )
            .filter(|entity| !run.asked.contains(entity))
            .collect();
        if entities.is_empty() {
            return Ok(());
        }
        let roots: Vec<(usize, PathBuf)> = staging
            .inventory_roots
            .iter()
            .chain([&self.playbook_dir])
            .cloned()
            .enumerate()
            .collect();
        self.fill_vars_plugins(&roots, &entities.into_iter().collect::<Vec<_>>())
    }

    /// The plugin maps of the playbook's root, emptied and asked again for every group and host
    /// asked so far: what a playbook in another directory reads.
    pub(super) fn rebase_vars_plugins(&mut self) -> anyhow::Result<()> {
        let Some(run) = self.vars_plugins.as_ref().and_then(|s| s.run.as_ref()) else {
            return Ok(());
        };
        let (before, after) = run.split();
        let width = before + 1 + after;
        let root = self.group_files.len() / width - 1;
        let entities: Vec<Entity> = run.asked.iter().cloned().collect();
        for files in [&mut self.group_files, &mut self.host_files] {
            for (slot, map) in files.iter_mut().enumerate().skip(root * width) {
                if slot != root * width + before {
                    map.clear();
                }
            }
        }
        self.fill_vars_plugins(&[(root, self.playbook_dir.clone())], &entities)
    }

    /// The inventory stage again, on the inventory a refresh read: the reference parses its
    /// sources anew, and runs the plugins with them, `host_group_vars` reading its files again.
    pub(super) fn refresh_vars_plugins(&mut self) -> anyhow::Result<()> {
        let Some(staging) = self.vars_plugins.as_mut() else {
            return Ok(());
        };
        let mut files = Vec::new();
        if staging.host_group_vars.inventory {
            for root in &staging.inventory_roots {
                files.push((
                    load_vars_dir(&root.join("group_vars"))?,
                    load_vars_dir(&root.join("host_vars"))?,
                ));
            }
        }
        staging.inventory_stage(&mut self.dynamic, &self.playbook_dir, &files)?;
        Ok(())
    }

    /// Asks the plugins about `entities` at the run's stage on each root, and lays what they
    /// answer in the root's plugin maps.
    fn fill_vars_plugins(
        &mut self,
        roots: &[(usize, PathBuf)],
        entities: &[Entity],
    ) -> anyhow::Result<()> {
        let Some(run) = self.vars_plugins.as_mut().and_then(|s| s.run.as_mut()) else {
            return Ok(());
        };
        let stage = run.stage;
        let calls: Vec<_> = roots
            .iter()
            .map(|(_, path)| (stage, path.as_path(), entities))
            .collect();
        let answers = run.ask(&self.dynamic, &self.playbook_dir, &calls)?;
        let (before, after) = run.split();
        let width = before + 1 + after;
        for ((root, _), answer) in roots.iter().zip(answers) {
            for (at, index, vars) in answer {
                let (group, name) = &entities[at];
                let slot = root * width + index + usize::from(index >= before);
                let files = if *group {
                    &mut self.group_files
                } else {
                    &mut self.host_files
                };
                let vars = run.tagged(index, &vars);
                files[slot].entry(name.clone()).or_default().extend(&vars);
            }
        }
        run.asked.extend(entities.iter().cloned());
        self.forget_hostvars();
        self.shared = None;
        Ok(())
    }
}
