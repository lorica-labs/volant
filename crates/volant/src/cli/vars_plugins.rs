// SPDX-License-Identifier: GPL-3.0-or-later
//! The vars plugins a command runs before it reads a variable.

use std::path::PathBuf;
use std::sync::Arc;

use crate::bridge::{Bridge, BridgeError};
use crate::config::VarsPlugins;
use crate::inventory::{PluginConfig, Source};
use crate::stats::Refusal;
use crate::vars::{HostGroupVars, VarStore};

/// Tells the inventory's `constructed` sources what `use_vars_plugins` runs at the stage `all`:
/// whether `host_group_vars` does, and whether `plugins` and `dirs` may load any other plugin.
pub(super) fn for_inventory(inventory: &mut PluginConfig, plugins: &VarsPlugins, dirs: &[PathBuf]) {
    inventory.host_group_vars_at_all = plugins.host_group_vars_runs("all");
    inventory.other_vars_plugins = !plugins.found(dirs).is_empty();
}

/// [`load_at`] the task stage, as a playbook run reads its variables.
pub(super) fn load(
    store: &mut VarStore,
    plugins: &VarsPlugins,
    dirs: &[PathBuf],
    bridge: &Arc<Bridge>,
    sources: &[Source],
) -> anyhow::Result<()> {
    load_at(store, plugins, dirs, bridge, sources, "task")
}

/// Runs the vars plugins `plugins` and `dirs` may load, at the inventory stage and at `stage`,
/// and lets `host_group_vars` read its files at the stages it runs at. A run that may load no
/// plugin besides it starts no Python. A plugin that raises, or a helper that cannot start, stops
/// the command before it reads a variable.
pub(super) fn load_at(
    store: &mut VarStore,
    plugins: &VarsPlugins,
    dirs: &[PathBuf],
    bridge: &Arc<Bridge>,
    sources: &[Source],
    stage: &'static str,
) -> anyhow::Result<()> {
    let found = plugins.found(dirs);
    let host_group_vars = HostGroupVars {
        inventory: plugins.host_group_vars_runs("inventory"),
        task: plugins.host_group_vars_runs(stage),
    };
    let bridge = (!found.is_empty()).then(|| Arc::clone(bridge));
    store
        .load_vars_plugins(sources, stage, host_group_vars, bridge)
        .map_err(|err| match err.downcast_ref::<BridgeError>() {
            Some(BridgeError::Start(why)) => Refusal::at(
                1,
                format!(
                    "The vars plugins {} need ansible-core, and the Python helper could not start: {why}",
                    found.join(", ")
                ),
            ),
            Some(other) => Refusal::at(1, other),
            None => err,
        })
}
