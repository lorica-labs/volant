// SPDX-License-Identifier: GPL-3.0-or-later
//! The handful of `ansible.cfg` settings this release reads, found where ansible-playbook
//! looks for them, with the matching environment variables taking precedence.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::executor::DEFAULT_CONNECT_TIMEOUT;

mod parse;
mod paths;
mod settings;

use parse::parse;
use paths::{default_collections_path, default_roles_path, expand_home, path_list};
use settings::{
    boolean, cache_plugin, forks, gathering, integer, runtime_mode, switch, timeout_secs,
};

/// Ansible's own default for `remote_tmp`, where the agent is cached on a host.
pub const DEFAULT_REMOTE_TMP: &str = "~/.ansible/tmp";
/// Ansible's own default for `forks`: how many hosts a play runs at once.
pub const DEFAULT_FORKS: usize = 5;

/// `async_dir` of the configuration the run loaded, for the code that names a job's directory
/// without a `Config` in hand.
static ASYNC_DIR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The directory `ansible.cfg` names for jobs, if it names one.
pub(crate) fn configured_async_dir() -> Option<String> {
    ASYNC_DIR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
/// Ansible's own defaults for `[privilege_escalation]`.
pub const DEFAULT_BECOME_USER: &str = "root";
pub const DEFAULT_BECOME_METHOD: &str = "sudo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `[defaults] inventory` or `ANSIBLE_INVENTORY`: every source, in order.
    pub inventory: Vec<PathBuf>,
    /// The `[inventory]` settings the plugin chain reads.
    pub inventory_plugins: crate::inventory::PluginConfig,
    pub timeout: Duration,
    pub remote_user: Option<String>,
    pub private_key_file: Option<PathBuf>,
    pub host_key_checking: bool,
    pub remote_tmp: String,
    /// `[defaults] async_dir`: where a job's status file goes when neither `ansible_async_dir`
    /// nor `ANSIBLE_ASYNC_DIR` names a directory.
    pub async_dir: Option<String>,
    pub forks: usize,
    pub r#become: bool,
    pub become_user: String,
    pub become_method: String,
    /// `become_exe` and `become_flags`, as written; `None` leaves the method's own program and
    /// flags in place.
    pub become_exe: Option<String>,
    pub become_flags: Option<String>,
    /// `vault_password_file` and `vault_identity_list`: where the vault secrets come from besides
    /// the command line.
    pub vault_password_file: Option<PathBuf>,
    pub vault_identity_list: Vec<String>,
    /// Whether a host that failed still runs the handlers it notified. Measured on ansible-core
    /// 2.19.12: `[defaults] force_handlers = True` and `--force-handlers` give the same run, down
    /// to the recap.
    pub force_handlers: bool,
    /// `[defaults] show_custom_stats` or `ANSIBLE_SHOW_CUSTOM_STATS`: print what `set_stats`
    /// collected after the recap. Off by default, as in the reference.
    pub show_custom_stats: bool,
    /// `[defaults] enable_task_debugger` or `ANSIBLE_ENABLE_TASK_DEBUGGER`: a failed or
    /// unreachable task with no `debugger` keyword opens the debugger prompt.
    pub enable_task_debugger: bool,
    /// Where a role is looked for once the directory beside the playbook has been tried.
    /// Measured on ansible-core 2.19.12: a `roles_path` **replaces** the three default
    /// directories rather than adding to them, and it never displaces `<playbook_dir>/roles`,
    /// which is searched first whatever it says.
    pub roles_path: Vec<PathBuf>,
    /// Where a collection is looked for, for the three-part role names.
    pub collections_path: Vec<PathBuf>,
    /// `[tags] run` and `[tags] skip`, or `ANSIBLE_RUN_TAGS` and `ANSIBLE_SKIP_TAGS`.
    ///
    /// Measured on ansible-core 2.19.12: these are the **default** of `--tags` and
    /// `--skip-tags`, and both options append to their default rather than replacing it, so
    /// `[tags] run = x` with `--tags y` on the command line runs the union `x, y` and not `y`
    /// alone. The command line never narrows what the file asked for; it only adds to it.
    pub tags_run: Vec<String>,
    pub tags_skip: Vec<String>,
    /// `[volant] batching`, or `VOLANT_BATCHING`: whether a host may carry on through the tasks
    /// between two synchronisation points instead of meeting the other hosts in front of every
    /// one of them. Off by default, which is what `linear` means. On, a host meets the others
    /// only in front of a task that reads what another host did, as far as the playbook's text
    /// shows it (see `is_boundary` in the executor); a dependency no text carries - a service
    /// another host is still installing, a file on a host named literally - is not seen, which
    /// is why it is asked for and never assumed.
    ///
    /// Not a playbook keyword, and never one: a playbook has to stay runnable by the reference
    /// as written, and the reference has no such keyword. Measured on ansible-core 2.19.12 with
    /// a `[volant]` section in the file: `ansible-playbook` runs the play and exits 0 with no
    /// warning and `ansible-config dump` does not list it, so one `ansible.cfg` drives both
    /// engines. `ansible-config validate` is the one exception - it answers `Found unknown
    /// section 'volant'` and exits 1.
    pub batching: bool,
    /// `[volant] native_modules`, or `VOLANT_NATIVE_MODULES`: whether an agent may answer a task
    /// with its native module instead of running the task's Python module. On by default; off
    /// sends every Python task to Python, which is the switch to compare the two.
    pub native_modules: bool,
    /// `[volant] runtime`, or `VOLANT_RUNTIME`: `auto` (the default), `managed` or `system`,
    /// which Python ansible-core runs under (see `runtime::resolve`).
    pub runtime: crate::runtime::Mode,
    /// `[volant] ssh_control_master`, or `VOLANT_SSH_CONTROL_MASTER`: whether the `ssh` runs
    /// of one inventory host share one connection. On by default.
    pub ssh_control_master: bool,
    /// `fact_caching` (`ANSIBLE_CACHE_PLUGIN`): `memory`, the default, or `jsonfile`.
    pub fact_caching: String,
    /// `fact_caching_connection`: the directory `jsonfile` keeps its files in.
    pub fact_caching_connection: Option<PathBuf>,
    /// `fact_caching_timeout`, in seconds; `0` never expires, as in the reference.
    pub fact_caching_timeout: i64,
    /// `fact_caching_prefix`: put in front of every file name.
    pub fact_caching_prefix: String,
    /// `gathering`: `implicit`, `explicit` or `smart`.
    pub gathering: String,
}

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
            force_handlers: false,
            show_custom_stats: false,
            enable_task_debugger: false,
            roles_path: default_roles_path(),
            collections_path: default_collections_path(),
            tags_run: Vec::new(),
            tags_skip: Vec::new(),
            batching: false,
            native_modules: true,
            runtime: crate::runtime::Mode::Auto,
            ssh_control_master: true,
            fact_caching: "memory".to_string(),
            fact_caching_connection: None,
            fact_caching_timeout: 86400,
            fact_caching_prefix: String::new(),
            gathering: "implicit".to_string(),
        }
    }
}

/// A comma-separated list of tags, as the reference reads one from a file, from the environment
/// or from the command line: split on commas, each piece trimmed, empty pieces dropped.
pub fn tag_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

impl Config {
    /// Reads the configuration file, if there is one, and lets the environment override it.
    ///
    /// A file that is there and cannot be read is ignored, and the run goes on with the
    /// defaults and the environment - which is what the reference does, measured: an
    /// `ansible.cfg` at mode 000 changes nothing there and the playbook exits 0. The one
    /// divergence kept is the warning: a configuration file the operator wrote and the process
    /// cannot open is worth a line, and a warning changes no exit code.
    #[expect(clippy::too_many_lines)]
    pub fn load() -> anyhow::Result<Config> {
        let mut config = match locate() {
            Some(path) => match std::fs::read_to_string(&path) {
                Ok(text) => parse(
                    &text,
                    path.parent().unwrap_or(Path::new(".")),
                    &path.display().to_string(),
                )?,
                Err(err) => {
                    eprintln!(
                        "[WARNING]: {} could not be read and was ignored: {err}",
                        path.display()
                    );
                    Config::default()
                }
            },
            None => Config::default(),
        };
        if let Ok(inv) = std::env::var("ANSIBLE_INVENTORY") {
            config.inventory = tag_list(&inv).iter().map(PathBuf::from).collect();
        }
        let plugins = &mut config.inventory_plugins;
        for (name, list) in [
            ("ANSIBLE_INVENTORY_ENABLED", &mut plugins.enabled),
            ("ANSIBLE_INVENTORY_IGNORE", &mut plugins.ignore_extensions),
            (
                "ANSIBLE_INVENTORY_IGNORE_REGEX",
                &mut plugins.ignore_patterns,
            ),
        ] {
            if let Ok(value) = std::env::var(name) {
                *list = tag_list(&value);
            }
        }
        for (env, setting, flag) in [
            (
                "ANSIBLE_INVENTORY_UNPARSED_FAILED",
                "INVENTORY_UNPARSED_IS_FAILED",
                &mut plugins.unparsed_is_failed,
            ),
            (
                "ANSIBLE_INVENTORY_ANY_UNPARSED_IS_FAILED",
                "INVENTORY_ANY_UNPARSED_IS_FAILED",
                &mut plugins.any_unparsed_is_failed,
            ),
            (
                "ANSIBLE_INVENTORY_PLUGIN_SCRIPT_STDERR",
                "always_show_stderr",
                &mut plugins.script_stderr,
            ),
        ] {
            if let Ok(value) = std::env::var(env) {
                *flag = boolean(setting, &format!("env: {env}"), &value)?;
            }
        }
        if let Ok(t) = std::env::var("ANSIBLE_TIMEOUT") {
            let origin = "env: ANSIBLE_TIMEOUT";
            config.timeout = Duration::from_secs(timeout_secs(
                integer("DEFAULT_TIMEOUT", origin, &t)?,
                origin,
            )?);
        }
        if let Ok(user) = std::env::var("ANSIBLE_REMOTE_USER") {
            config.remote_user = Some(user);
        }
        if let Ok(key) = std::env::var("ANSIBLE_PRIVATE_KEY_FILE") {
            config.private_key_file = Some(PathBuf::from(key));
        }
        if let Ok(flag) = std::env::var("ANSIBLE_HOST_KEY_CHECKING")
            && let Some(on) = crate::yaml::bool_from_str(flag.trim())
        {
            config.host_key_checking = on;
        }
        // A blank value is refused here as it is in the file: an empty `remote_tmp` makes the
        // remote `mkdir` fail on an empty path, and the run then blames the upload.
        if let Ok(tmp) = std::env::var("ANSIBLE_REMOTE_TMP")
            && !tmp.trim().is_empty()
        {
            let tmp = tmp.trim();
            crate::transport::validate_remote_tmp(tmp)
                .map_err(|e| crate::stats::Refusal::at(5, e))?;
            config.remote_tmp = tmp.to_string();
        }
        // A zero, and a negative value with it, are kept rather than dropped: the reference
        // refuses them from every source and the single check at startup is what says so
        // (exit 2, measured). A value that is not an integer at all is a different refusal
        // with a different code, and `integer` raises it.
        if let Ok(n) = std::env::var("ANSIBLE_FORKS") {
            config.forks = forks(integer("DEFAULT_FORKS", "env: ANSIBLE_FORKS", &n)?);
        }
        if let Ok(flag) = std::env::var("ANSIBLE_BECOME")
            && let Some(on) = crate::yaml::bool_from_str(flag.trim())
        {
            config.r#become = on;
        }
        if let Ok(flag) = std::env::var("ANSIBLE_FORCE_HANDLERS")
            && let Some(on) = crate::yaml::bool_from_str(flag.trim())
        {
            config.force_handlers = on;
        }
        if let Ok(flag) = std::env::var("ANSIBLE_SHOW_CUSTOM_STATS")
            && let Some(on) = crate::yaml::bool_from_str(flag.trim())
        {
            config.show_custom_stats = on;
        }
        if let Ok(flag) = std::env::var("ANSIBLE_ENABLE_TASK_DEBUGGER")
            && let Some(on) = crate::yaml::bool_from_str(flag.trim())
        {
            config.enable_task_debugger = on;
        }
        TASK_DEBUGGER.store(config.enable_task_debugger, Ordering::Relaxed);
        let cwd = std::env::current_dir().unwrap_or_default();
        if let Ok(value) = std::env::var("ANSIBLE_CACHE_PLUGIN") {
            config.fact_caching = cache_plugin(&value, "env: ANSIBLE_CACHE_PLUGIN")?;
        }
        if let Ok(value) = std::env::var("ANSIBLE_CACHE_PLUGIN_CONNECTION") {
            config.fact_caching_connection = Some(expand_home(value.trim(), &cwd));
        }
        if let Ok(value) = std::env::var("ANSIBLE_CACHE_PLUGIN_TIMEOUT") {
            config.fact_caching_timeout = integer(
                "CACHE_PLUGIN_TIMEOUT",
                "env: ANSIBLE_CACHE_PLUGIN_TIMEOUT",
                &value,
            )?;
        }
        if let Ok(value) = std::env::var("ANSIBLE_CACHE_PLUGIN_PREFIX") {
            config.fact_caching_prefix = value;
        }
        if let Ok(value) = std::env::var("ANSIBLE_GATHERING") {
            config.gathering = gathering(&value, "env: ANSIBLE_GATHERING")?;
        }
        if let Ok(value) = std::env::var("VOLANT_BATCHING")
            && let Some(on) = switch(&value)
        {
            config.batching = on;
        }
        if let Ok(value) = std::env::var("VOLANT_NATIVE_MODULES") {
            config.native_modules = boolean(
                "VOLANT_NATIVE_MODULES",
                "env: VOLANT_NATIVE_MODULES",
                &value,
            )?;
        }
        if let Ok(value) = std::env::var("VOLANT_RUNTIME") {
            config.runtime = runtime_mode(&value, "env: VOLANT_RUNTIME")?;
        }
        if let Ok(value) = std::env::var("VOLANT_SSH_CONTROL_MASTER")
            && let Some(on) = switch(&value)
        {
            config.ssh_control_master = on;
        }
        // Measured: the environment replaces the file's `roles_path` rather than being appended
        // to it, and a relative entry is read against the working directory.
        let from_env = |name: &str| -> Option<Vec<PathBuf>> {
            let list = path_list(&std::env::var(name).ok()?, Path::new(""));
            (!list.is_empty()).then_some(list)
        };
        if let Some(list) = from_env("ANSIBLE_ROLES_PATH") {
            config.roles_path = list;
        }
        if let Some(list) =
            from_env("ANSIBLE_COLLECTIONS_PATHS").or_else(|| from_env("ANSIBLE_COLLECTIONS_PATH"))
        {
            config.collections_path = list;
        }
        if let Ok(user) = std::env::var("ANSIBLE_BECOME_USER")
            && !user.trim().is_empty()
        {
            config.become_user = user.trim().to_string();
        }
        // Kept exactly as written, so the escalation check refuses an unsupported method by
        // name rather than falling back to `sudo` for an operator who asked for something else.
        // A blank value is no such request: an unset variable an exporting shell passed on as
        // an empty one leaves the default alone, the way an empty `become_user` does.
        if let Ok(method) = std::env::var("ANSIBLE_BECOME_METHOD")
            && !method.trim().is_empty()
        {
            config.become_method = method.trim().to_string();
        }
        if let Ok(exe) = std::env::var("ANSIBLE_BECOME_EXE")
            && !exe.trim().is_empty()
        {
            config.become_exe = Some(exe.trim().to_string());
        }
        if let Ok(flags) = std::env::var("ANSIBLE_BECOME_FLAGS")
            && !flags.trim().is_empty()
        {
            config.become_flags = Some(flags.trim().to_string());
        }
        // The environment replaces the file's list rather than adding to it, the way every
        // other list setting here does; the command line is what adds to whichever won.
        if let Ok(path) = std::env::var("ANSIBLE_VAULT_PASSWORD_FILE")
            && !path.is_empty()
        {
            config.vault_password_file = Some(PathBuf::from(path));
        }
        if let Ok(list) = std::env::var("ANSIBLE_VAULT_IDENTITY_LIST") {
            config.vault_identity_list = tag_list(&list);
        }
        if let Ok(list) = std::env::var("ANSIBLE_RUN_TAGS") {
            config.tags_run = tag_list(&list);
        }
        if let Ok(list) = std::env::var("ANSIBLE_SKIP_TAGS") {
            config.tags_skip = tag_list(&list);
        }
        // After the user's own entries, so their collections shadow the ones Volant fetched.
        config.collections_path = crate::runtime::collections::after(
            config.collections_path,
            crate::runtime::collections::active_dir(),
        );
        let mut held = ASYNC_DIR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.clone_from(&config.async_dir);
        drop(held);
        Ok(config)
    }
}

/// `enable_task_debugger` as the last [`Config::load`] read it. The executor is handed the
/// options of a run, not its configuration, and this is the one setting the debugger reads.
/// One value per process: each load overwrites it, so two configurations loaded side by side
/// share whichever came last, which holds while a process runs one playbook command.
static TASK_DEBUGGER: AtomicBool = AtomicBool::new(false);

/// Whether `enable_task_debugger` is on; off when no configuration was loaded.
pub(crate) fn task_debugger() -> bool {
    TASK_DEBUGGER.load(Ordering::Relaxed)
}

/// `ANSIBLE_CONFIG`, then `./ansible.cfg`, `~/.ansible.cfg`, `/etc/ansible/ansible.cfg`.
pub(crate) fn locate() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("ANSIBLE_CONFIG") {
        let p = PathBuf::from(explicit);
        return p.is_file().then_some(p);
    }
    let mut candidates = vec![PathBuf::from("ansible.cfg")];
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".ansible.cfg"));
    }
    candidates.push(PathBuf::from("/etc/ansible/ansible.cfg"));
    candidates.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests;
