// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading the sections of a configuration file.

use std::path::Path;
use std::time::Duration;

use super::paths::{expand_home, path_list};
use super::settings::{
    boolean, cache_plugin, callback_flag, forks, gathering, host_named_dest_dirs, host_named_paths,
    integer, runtime_mode, switch, timeout_secs,
};
use super::{Config, tag_list};

/// Reads `[defaults]` and `[privilege_escalation]`. Relative paths are relative to the
/// configuration file; `origin` names the file in a refusal, as the reference names it.
#[expect(clippy::too_many_lines)]
pub(super) fn parse(text: &str, base: &Path, origin: &str) -> anyhow::Result<Config> {
    let mut config = Config::default();
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if section == "privilege_escalation" {
            match key.trim() {
                "become" => {
                    if let Some(on) = crate::yaml::bool_from_str(value.trim()) {
                        config.r#become = on;
                    }
                }
                "become_user" => {
                    let user = value.trim();
                    if !user.is_empty() {
                        config.become_user = user.to_string();
                    }
                }
                // Kept as written so an unsupported method is refused by name, but a blank
                // `become_method =` line asks for nothing and leaves the default alone.
                "become_method" => {
                    let method = value.trim();
                    if !method.is_empty() {
                        config.become_method = method.to_string();
                    }
                }
                "become_exe" => {
                    let exe = value.trim();
                    if !exe.is_empty() {
                        config.become_exe = Some(exe.to_string());
                    }
                }
                // A blank `become_flags =` is kept: the reference then runs the program with no
                // flags at all, its own defaults included.
                "become_flags" => config.become_flags = Some(value.trim().to_string()),
                _ => {}
            }
            continue;
        }
        if section == "tags" {
            match key.trim() {
                "run" => config.tags_run = tag_list(value),
                "skip" => config.tags_skip = tag_list(value),
                _ => {}
            }
            continue;
        }
        // Volant's own section, which the reference ignores along with every other section it
        // does not know: an `ansible.cfg` shared by the two engines stays valid for both.
        if section == "volant" {
            match key.trim() {
                "batching" => {
                    if let Some(on) = switch(value) {
                        config.batching = on;
                    }
                }
                "host_named_paths" => config.host_named_paths = host_named_paths(value, origin)?,
                "host_named_dest_dirs" => {
                    config.host_named_dest_dirs = host_named_dest_dirs(value, origin)?;
                }
                "native_modules" => {
                    config.native_modules = boolean("VOLANT_NATIVE_MODULES", origin, value)?;
                }
                "runtime" => config.runtime = runtime_mode(value, origin)?,
                "ssh_control_master" => {
                    if let Some(on) = switch(value) {
                        config.ssh_control_master = on;
                    }
                }
                _ => {}
            }
            continue;
        }
        let plugins = &mut config.inventory_plugins;
        match (section.as_str(), key.trim()) {
            ("inventory", "enable_plugins") => plugins.enabled = tag_list(value),
            ("inventory", "ignore_extensions") | ("defaults", "inventory_ignore_extensions") => {
                plugins.ignore_extensions = tag_list(value);
            }
            ("inventory", "ignore_patterns") | ("defaults", "inventory_ignore_patterns") => {
                plugins.ignore_patterns = tag_list(value);
            }
            ("inventory", "unparsed_is_failed") => {
                plugins.unparsed_is_failed =
                    boolean("INVENTORY_UNPARSED_IS_FAILED", origin, value.trim())?;
            }
            ("inventory", "any_unparsed_is_failed") => {
                plugins.any_unparsed_is_failed =
                    boolean("INVENTORY_ANY_UNPARSED_IS_FAILED", origin, value.trim())?;
            }
            ("inventory_plugin_script", "always_show_stderr") => {
                plugins.script_stderr = boolean("always_show_stderr", origin, value.trim())?;
            }
            _ => {}
        }
        if section != "defaults" {
            continue;
        }
        match key.trim() {
            "inventory" => {
                config.inventory = tag_list(value)
                    .iter()
                    .map(|p| expand_home(p, base))
                    .collect();
            }
            "timeout" => {
                config.timeout = Duration::from_secs(timeout_secs(
                    integer("DEFAULT_TIMEOUT", origin, value)?,
                    origin,
                )?);
            }
            "remote_user" => {
                let user = value.trim();
                if !user.is_empty() {
                    config.remote_user = Some(user.to_string());
                }
            }
            "vault_password_file" => {
                let path = value.trim();
                if !path.is_empty() {
                    config.vault_password_file = Some(expand_home(path, base));
                }
            }
            "vault_identity_list" => config.vault_identity_list = tag_list(value),
            "private_key_file" => {
                let path = value.trim();
                if !path.is_empty() {
                    config.private_key_file = Some(base.join(path));
                }
            }
            "host_key_checking" => {
                if let Some(on) = crate::yaml::bool_from_str(value.trim()) {
                    config.host_key_checking = on;
                }
            }
            "force_handlers" => {
                if let Some(on) = crate::yaml::bool_from_str(value.trim()) {
                    config.force_handlers = on;
                }
            }
            "localhost_warning" => {
                config.localhost_warning = boolean("LOCALHOST_WARNING", origin, value.trim())?;
            }
            "show_custom_stats" => {
                if let Some(on) = crate::yaml::bool_from_str(value.trim()) {
                    config.show_custom_stats = on;
                }
            }
            "show_task_path_on_failure" => {
                config.show_task_path_on_failure = callback_flag(value);
            }
            "enable_task_debugger" => {
                if let Some(on) = crate::yaml::bool_from_str(value.trim()) {
                    config.enable_task_debugger = on;
                }
            }
            "async_dir" => {
                let dir = value.trim();
                if !dir.is_empty() {
                    config.async_dir = Some(dir.to_string());
                }
            }
            "remote_tmp" => {
                let tmp = value.trim();
                if !tmp.is_empty() {
                    crate::transport::validate_remote_tmp(tmp)
                        .map_err(|e| crate::stats::Refusal::at(5, e))?;
                    config.remote_tmp = tmp.to_string();
                }
            }
            // A zero reaches the caller as zero, where the single startup check refuses it.
            // A value that is no number at all is the reference's own exit 5 instead.
            "forks" => config.forks = forks(integer("DEFAULT_FORKS", origin, value)?),
            "fact_caching" => config.fact_caching = cache_plugin(value, origin)?,
            "fact_caching_connection" => {
                config.fact_caching_connection = Some(expand_home(value.trim(), base));
            }
            "fact_caching_timeout" => {
                config.fact_caching_timeout = integer("CACHE_PLUGIN_TIMEOUT", origin, value)?;
            }
            "fact_caching_prefix" => config.fact_caching_prefix = value.trim().to_string(),
            "gathering" => config.gathering = gathering(value, origin)?,
            "roles_path" => {
                let list = path_list(value, base);
                if !list.is_empty() {
                    config.roles_path = list;
                }
            }
            "collections_path" | "collections_paths" => {
                let list = path_list(value, base);
                if !list.is_empty() {
                    config.collections_path = list;
                }
            }
            _ => {}
        }
    }
    Ok(config)
}
