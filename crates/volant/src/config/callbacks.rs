// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings that choose the callbacks a run prints through: `stdout_callback`,
//! `callbacks_enabled`, `callback_plugins` and `bin_ansible_callbacks`, and `callback_format_pretty`.

use super::settings::{boolean, callback_flag, integer};
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
    /// `ANSIBLE_CALLBACK_FORMAT_PRETTY`, else `[defaults] callback_format_pretty`: the
    /// `pretty_results` of `default` and `minimal`. A word that is no boolean reads as false.
    pub pretty_results: Option<bool>,
    /// `ANSIBLE_JSON_INDENT`, else `[defaults] json_indent`: the `json` callback's indent.
    pub json_indent: Option<i64>,
    /// The keys of `[callback_tree]` and `[callback_profile_tasks]`, by section and key.
    pub sections: std::collections::BTreeMap<(String, String), String>,
}

impl Callbacks {
    /// An option of a callback that prints beside the stdout one: `env` when it is set, else
    /// `key` of `[section]`.
    pub fn option(&self, env: &str, section: &str, key: &str) -> Option<String> {
        std::env::var(env).ok().or_else(|| {
            self.sections
                .get(&(section.to_string(), key.to_string()))
                .cloned()
        })
    }
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
        "json_indent" => callbacks.json_indent = Some(integer("json_indent", origin, value)?),
        "callback_format_pretty" => callbacks.pretty_results = Some(callback_flag(value)),
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
    if let Ok(indent) = std::env::var("ANSIBLE_JSON_INDENT") {
        callbacks.json_indent = Some(integer("json_indent", "env: ANSIBLE_JSON_INDENT", &indent)?);
    }
    if let Ok(flag) = std::env::var("ANSIBLE_CALLBACK_FORMAT_PRETTY") {
        callbacks.pretty_results = Some(callback_flag(&flag));
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
