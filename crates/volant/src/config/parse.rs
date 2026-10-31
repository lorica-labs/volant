// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading the sections of a configuration file.

use std::path::Path;
use std::time::Duration;

use super::paths::{expand_home, path_list};
use super::settings::{
    boolean, cache_plugin, callback_flag, forks, gathering, host_named_dest_dirs, host_named_paths,
    integer, runtime_mode, shadow_coverage, shadow_runs, switch, timeout_secs,
};
use super::{Config, tag_list};

/// `[defaults] module_strict_utf8_response`, on unless the file turns it off.
static STRICT_UTF8: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// `MODULE_STRICT_UTF8_RESPONSE`: whether `from_json` refuses a string with a lone surrogate.
/// `ANSIBLE_MODULE_STRICT_UTF8_RESPONSE` wins over the file.
pub(crate) fn strict_utf8() -> bool {
    std::env::var("ANSIBLE_MODULE_STRICT_UTF8_RESPONSE")
        .ok()
        .and_then(|flag| python_bool(&flag))
        .unwrap_or_else(|| STRICT_UTF8.load(std::sync::atomic::Ordering::Relaxed))
}

/// ansible-core's `boolean()` for a setting: `1`, `yes`, `on`, `true` and the rest, any case.
fn python_bool(word: &str) -> Option<bool> {
    match word.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "on" | "1" | "true" | "t" | "1.0" => Some(true),
        "n" | "no" | "off" | "0" | "false" | "f" | "0.0" => Some(false),
        _ => None,
    }
}

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
        if section == "galaxy" || section.starts_with("galaxy_server.") {
            config
                .galaxy
                .push((section.clone(), key.trim().into(), value.trim().into()));
            continue;
        }
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
        if section == "callback_tree" || section == "callback_profile_tasks" {
            config.callbacks.sections.insert(
                (section.clone(), key.trim().to_string()),
                value.trim().to_string(),
            );
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
                "shadow_runs" => config.shadow_runs = shadow_runs(value, origin)?,
                "shadow_coverage" => config.shadow_coverage = shadow_coverage(value, origin)?,
                "locked" => config.locked = boolean("locked", origin, value)?,
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
            ("inventory", "export") => {
                plugins.export = boolean("INVENTORY_EXPORT", origin, value.trim())?;
            }
            ("inventory_plugin_script", "always_show_stderr") => {
                plugins.script_stderr = boolean("always_show_stderr", origin, value.trim())?;
            }
            _ => {}
        }
        if section == "ssh_connection" && key.trim() == "usetty" {
            config.ssh_usetty = callback_flag(value);
        }
        if section == "docker_connection" {
            super::docker::ini(&mut config.plugins.docker, key.trim(), value, origin)?;
        }
        if section == "chroot_connection" {
            super::docker::chroot_ini(&mut config.plugins.chroot, key.trim(), value, origin)?;
        }
        if section == "paramiko_connection" {
            super::paramiko::ini(&mut config.plugins.paramiko, key.trim(), value, origin)?;
        }
        if section == "ssh_connection" && key.trim() == "timeout" {
            config.plugins.paramiko.ssh_timeout = Some(super::paramiko::seconds(origin, value)?);
        }
        if section != "defaults" {
            continue;
        }
        match key.trim() {
            "podman_extra_args" => {
                config.plugins.podman.extra_args = Some(value.trim().to_string());
            }
            "executable" => config.plugins.chroot.executable = Some(value.trim().to_string()),
            "remote_port" => {
                config.plugins.paramiko.defaults_port = Some(super::paramiko::port(origin, value)?);
            }
            _ => {}
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
            "vault_encrypt_identity" => {
                let id = value.trim();
                if !id.is_empty() {
                    config.vault_encrypt_identity = Some(id.to_string());
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
            "private_role_vars" => {
                if let Some(private) = crate::yaml::bool_from_str(value.trim()) {
                    crate::roles::set_private_role_vars(private);
                }
            }
            "hash_behaviour" => crate::vars::set_hash_merge(value.trim() == "merge"),
            "force_handlers" => {
                if let Some(on) = crate::yaml::bool_from_str(value.trim()) {
                    config.force_handlers = on;
                }
            }
            "localhost_warning" => {
                config.localhost_warning = boolean("LOCALHOST_WARNING", origin, value.trim())?;
            }
            "duplicate_dict_key" => config.duplicate_dict_key = Some(value.trim().to_string()),
            "facts_modules" => config.facts_modules = tag_list(value.trim()),
            "module_strict_utf8_response" => {
                if let Some(on) = python_bool(value) {
                    STRICT_UTF8.store(on, std::sync::atomic::Ordering::Relaxed);
                }
            }
            "show_custom_stats" => {
                if let Some(on) = crate::yaml::bool_from_str(value.trim()) {
                    config.show_custom_stats = on;
                }
            }
            "force_color" => config.force_color = boolean("COLOR_FORCE", origin, value.trim())?,
            "nocolor" => config.nocolor = boolean("NOCOLOR", origin, value.trim())?,
            "deprecation_warnings" => {
                config.deprecation_warnings =
                    boolean("DEPRECATION_WARNINGS", origin, value.trim())?;
            }
            "log_path" => config.log_path = Some(expand_home(value.trim(), base)),
            "plugin_filters_cfg" => super::filters::load(&expand_home(value.trim(), base)),
            "log_verbosity" => {
                let level = integer("DEFAULT_LOG_VERBOSITY", origin, value.trim())?;
                config.log_verbosity = u8::try_from(level.max(0)).unwrap_or(u8::MAX);
            }
            "callback_result_format" => {
                config.callback_result_format = Some(value.trim().to_string());
            }
            "show_task_path_on_failure" => {
                config.show_task_path_on_failure = callback_flag(value);
            }
            "stdout_callback"
            | "callbacks_enabled"
            | "callback_plugins"
            | "bin_ansible_callbacks"
            | "callback_format_pretty"
            | "json_indent" => {
                super::callbacks::ini(&mut config.callbacks, key.trim(), value, origin, base)?;
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
