// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings that choose the callbacks a run prints through: `stdout_callback`,
//! `callbacks_enabled`, `callback_plugins` and `bin_ansible_callbacks`.

use super::settings::boolean;
use std::path::{Path, PathBuf};

use super::paths::path_list;
use super::tag_list;

/// Which callbacks load, as `ansible.cfg` and the environment say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Callbacks {
    /// `ANSIBLE_STDOUT_CALLBACK`, else `[defaults] stdout_callback`; `default` when neither is set.
    pub stdout: Option<String>,
    /// `ANSIBLE_CALLBACKS_ENABLED`, else `[defaults] callbacks_enabled`, split on commas.
    pub enabled: Vec<String>,
    /// `ANSIBLE_CALLBACK_PLUGINS`, else `[defaults] callback_plugins`: more directories to look
    /// in. Read and kept; this engine runs only the callbacks it implements.
    pub plugin_dirs: Vec<PathBuf>,
    /// `ANSIBLE_LOAD_CALLBACK_PLUGINS`, else `[defaults] bin_ansible_callbacks`: whether the ad hoc
    /// command loads the configured callbacks.
    pub adhoc_loads: bool,
}

/// One key of `[defaults]`; a key that is not one of the four is left alone.
pub(super) fn ini(
    callbacks: &mut Callbacks,
    key: &str,
    value: &str,
    origin: &str,
    base: &Path,
) -> anyhow::Result<()> {
    match key {
        "stdout_callback" => callbacks.stdout = Some(value.trim().to_string()),
        "callbacks_enabled" => callbacks.enabled = tag_list(value),
        "callback_plugins" => callbacks.plugin_dirs = path_list(value, base),
        "bin_ansible_callbacks" => {
            callbacks.adhoc_loads = boolean("DEFAULT_LOAD_CALLBACK_PLUGINS", origin, value.trim())?;
        }
        _ => {}
    }
    Ok(())
}

/// The environment, which wins over the file.
pub(super) fn env(callbacks: &mut Callbacks) -> anyhow::Result<()> {
    if let Ok(name) = std::env::var("ANSIBLE_STDOUT_CALLBACK") {
        callbacks.stdout = Some(name.trim().to_string());
    }
    if let Ok(names) = std::env::var("ANSIBLE_CALLBACKS_ENABLED") {
        callbacks.enabled = tag_list(&names);
    }
    if let Ok(paths) = std::env::var("ANSIBLE_CALLBACK_PLUGINS") {
        callbacks.plugin_dirs = path_list(&paths, Path::new(""));
    }
    if let Ok(flag) = std::env::var("ANSIBLE_LOAD_CALLBACK_PLUGINS") {
        callbacks.adhoc_loads = boolean(
            "DEFAULT_LOAD_CALLBACK_PLUGINS",
            "env: ANSIBLE_LOAD_CALLBACK_PLUGINS",
            flag.trim(),
        )?;
    }
    Ok(())
}
