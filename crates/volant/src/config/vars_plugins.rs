// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings that choose the vars plugins a run loads: `vars_plugins`, `vars_plugins_enabled`
//! and `run_vars_plugins`.

use std::path::{Path, PathBuf};

use super::paths::{expand_home, path_list};
use super::tag_list;

/// Which vars plugins load, as `ansible.cfg` and the environment say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarsPlugins {
    /// `ANSIBLE_VARS_PLUGINS`, else `[defaults] vars_plugins`: the directories searched besides
    /// each playbook's `vars_plugins/`. ansible-core's default when neither is set.
    pub dirs: Vec<PathBuf>,
    /// `ANSIBLE_VARS_ENABLED`, else `[defaults] vars_plugins_enabled`, split on commas.
    pub enabled: Vec<String>,
    /// `ANSIBLE_RUN_VARS_PLUGINS`, else `[defaults] run_vars_plugins`: `demand` or `start`.
    pub run: String,
}

impl Default for VarsPlugins {
    fn default() -> Self {
        let cwd = Path::new("");
        Self {
            dirs: ["~/.ansible/plugins/vars", "/usr/share/ansible/plugins/vars"]
                .iter()
                .map(|dir| expand_home(dir, cwd))
                .collect(),
            enabled: vec!["host_group_vars".to_string()],
            run: "demand".to_string(),
        }
    }
}

/// One key of `[defaults]`; a key that is not one of the three is left alone.
pub(super) fn ini(plugins: &mut VarsPlugins, key: &str, value: &str, base: &Path) {
    match key {
        "vars_plugins" => plugins.dirs = path_list(value, base),
        "vars_plugins_enabled" => plugins.enabled = tag_list(value),
        "run_vars_plugins" => plugins.run = value.trim().to_string(),
        _ => {}
    }
}

/// The environment, which wins over the file.
pub(super) fn env(plugins: &mut VarsPlugins) {
    if let Ok(paths) = std::env::var("ANSIBLE_VARS_PLUGINS") {
        plugins.dirs = path_list(&paths, Path::new(""));
    }
    if let Ok(names) = std::env::var("ANSIBLE_VARS_ENABLED") {
        plugins.enabled = tag_list(&names);
    }
    if let Ok(word) = std::env::var("ANSIBLE_RUN_VARS_PLUGINS") {
        plugins.run = word.trim().to_string();
    }
}
