// SPDX-License-Identifier: GPL-3.0-or-later
//! What a [`Config`] holds when no file, variable or option sets a key: the reference's defaults.

use super::*;

impl Default for Config {
    fn default() -> Self {
        Self {
            inventory: Vec::new(),
            inventory_plugins: crate::inventory::PluginConfig::default(),
            timeout: DEFAULT_CONNECT_TIMEOUT,
            remote_user: None,
            private_key_file: None,
            host_key_checking: true,
            remote_tmp: DEFAULT_REMOTE_TMP.to_string(),
            async_dir: None,
            forks: DEFAULT_FORKS,
            r#become: false,
            become_user: DEFAULT_BECOME_USER.to_string(),
            become_method: DEFAULT_BECOME_METHOD.to_string(),
            become_exe: None,
            become_flags: None,
            vault_password_file: None,
            vault_identity_list: Vec::new(),
            vault_encrypt_identity: None,
            force_handlers: false,
            localhost_warning: true,
            show_custom_stats: false,
            show_task_path_on_failure: false,
            callback_result_format: None,
            force_color: false,
            nocolor: false,
            deprecation_warnings: true,
            ssh_usetty: true,
            enable_task_debugger: false,
            roles_path: default_roles_path(),
            collections_path: default_collections_path(),
            tags_run: Vec::new(),
            tags_skip: Vec::new(),
            batching: false,
            host_named_paths: HostNamedPaths::Allow,
            host_named_dest_dirs: None,
            native_modules: true,
            runtime: crate::runtime::Mode::Auto,
            ssh_control_master: true,
            shadow_runs: 5,
            shadow_coverage: 900,
            fact_caching: "memory".to_string(),
            fact_caching_connection: None,
            fact_caching_timeout: 86400,
            fact_caching_prefix: String::new(),
            gathering: "implicit".to_string(),
        }
    }
}
