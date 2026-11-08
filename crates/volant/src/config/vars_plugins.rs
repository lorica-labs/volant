// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings that choose the vars plugins a run loads and when they run: `vars_plugins`,
//! `vars_plugins_enabled`, `run_vars_plugins`, and the `stage` of `host_group_vars`.

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
    /// `ANSIBLE_VARS_PLUGIN_STAGE`, else `[vars_host_group_vars] stage`: the stage
    /// `host_group_vars` runs at, in place of `run`.
    pub host_group_vars_stage: Option<String>,
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
            host_group_vars_stage: None,
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
    if let Ok(word) = std::env::var("ANSIBLE_VARS_PLUGIN_STAGE") {
        plugins.host_group_vars_stage = Some(word.trim().to_string());
    }
}

impl VarsPlugins {
    /// Whether `host_group_vars` runs at `stage` (`inventory`, `task` or `all`), as
    /// `_plugin_should_run` decides: its own `stage` when set, else `run`, and never when
    /// `vars_plugins_enabled` leaves it out.
    pub fn host_group_vars_runs(&self, stage: &str) -> bool {
        let enabled = self.enabled.iter().any(|name| {
            matches!(
                name.as_str(),
                "host_group_vars" | "ansible.builtin.host_group_vars"
            )
        });
        match self
            .host_group_vars_stage
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            _ if !enabled => false,
            Some(allowed) => allowed == "all" || allowed == stage,
            None => {
                !(self.run == "demand" && stage == "inventory"
                    || self.run == "start" && stage == "task")
            }
        }
    }

    /// What may load as a vars plugin besides `host_group_vars`, which Volant reads itself: each
    /// other name `vars_plugins_enabled` gives, then each Python file of the configured
    /// directories and of the `vars_plugins/` of each of `dirs`. Empty when the run needs no
    /// helper for them.
    pub fn found(&self, dirs: &[PathBuf]) -> Vec<String> {
        let names = self
            .enabled
            .iter()
            .filter(|name| {
                !matches!(
                    name.as_str(),
                    "host_group_vars" | "ansible.builtin.host_group_vars"
                )
            })
            .cloned();
        let files = self
            .dirs
            .iter()
            .cloned()
            .chain(dirs.iter().map(|dir| dir.join("vars_plugins")))
            .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten())
            .map(|entry| entry.path())
            // The loader skips `__init__.py` and every other name that starts with `_`.
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "py")
                    && !path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('_'))
            })
            .map(|path| path.display().to_string());
        names.chain(files).collect()
    }
}
