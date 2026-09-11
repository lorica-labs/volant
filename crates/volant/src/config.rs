// SPDX-License-Identifier: GPL-3.0-or-later
//! The handful of `ansible.cfg` settings this release reads, found where ansible-playbook
//! looks for them, with the matching environment variables taking precedence.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::executor::DEFAULT_CONNECT_TIMEOUT;

/// Ansible's own default for `remote_tmp`, where the agent is cached on a host.
pub const DEFAULT_REMOTE_TMP: &str = "~/.ansible/tmp";
/// Ansible's own default for `forks`: how many hosts a play runs at once.
pub const DEFAULT_FORKS: usize = 5;
/// Ansible's own defaults for `[privilege_escalation]`.
pub const DEFAULT_BECOME_USER: &str = "root";
pub const DEFAULT_BECOME_METHOD: &str = "sudo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub inventory: Option<PathBuf>,
    pub timeout: Duration,
    pub remote_user: Option<String>,
    pub private_key_file: Option<PathBuf>,
    pub host_key_checking: bool,
    pub remote_tmp: String,
    pub forks: usize,
    pub r#become: bool,
    pub become_user: String,
    pub become_method: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            inventory: None,
            timeout: DEFAULT_CONNECT_TIMEOUT,
            remote_user: None,
            private_key_file: None,
            host_key_checking: true,
            remote_tmp: DEFAULT_REMOTE_TMP.to_string(),
            forks: DEFAULT_FORKS,
            r#become: false,
            become_user: DEFAULT_BECOME_USER.to_string(),
            become_method: DEFAULT_BECOME_METHOD.to_string(),
        }
    }
}

impl Config {
    /// Reads the configuration file, if there is one, and lets the environment override it.
    ///
    /// A file that is there and cannot be read is ignored, and the run goes on with the
    /// defaults and the environment - which is what the reference does, measured: an
    /// `ansible.cfg` at mode 000 changes nothing there and the playbook exits 0. The one
    /// divergence kept is the warning: a configuration file the operator wrote and the process
    /// cannot open is worth a line, and a warning changes no exit code.
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
            config.inventory = Some(PathBuf::from(inv));
        }
        if let Ok(t) = std::env::var("ANSIBLE_TIMEOUT") {
            config.timeout =
                Duration::from_secs(integer("DEFAULT_TIMEOUT", "env: ANSIBLE_TIMEOUT", &t)?);
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
            config.remote_tmp = tmp.trim().to_string();
        }
        // A zero, and a negative value with it, are kept rather than dropped: the reference
        // refuses them from every source and the single check at startup is what says so
        // (exit 2, measured). A value that is not an integer at all is a different refusal
        // with a different code, and `integer` raises it.
        if let Ok(n) = std::env::var("ANSIBLE_FORKS") {
            config.forks = integer("DEFAULT_FORKS", "env: ANSIBLE_FORKS", &n)? as usize;
        }
        if let Ok(flag) = std::env::var("ANSIBLE_BECOME")
            && let Some(on) = crate::yaml::bool_from_str(flag.trim())
        {
            config.r#become = on;
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
        Ok(config)
    }
}

/// `ANSIBLE_CONFIG`, then `./ansible.cfg`, `~/.ansible.cfg`, `/etc/ansible/ansible.cfg`.
fn locate() -> Option<PathBuf> {
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

/// An integer setting, or the reference's own refusal for one that is not an integer.
///
/// Measured against ansible-core 2.19.12 on the development machine: `forks = many` in
/// `ansible.cfg` prints `ERROR: Config 'DEFAULT_FORKS' from '<path>' has an invalid value:
/// Invalid value provided for 'integer': 'many'` and exits **5**, before any play header;
/// `timeout = abc` prints the same with `DEFAULT_TIMEOUT`, and `ANSIBLE_TIMEOUT=abc` prints it
/// with `env: ANSIBLE_TIMEOUT` as the origin. Silently keeping the default instead - which is
/// what `timeout` used to do from both its sources - runs the playbook with a value the
/// operator never wrote and the reference never accepted.
///
/// A negative number parses here and is clamped to zero, where the startup check refuses it
/// with the same words and the same exit 2 a literal zero gets.
fn integer(name: &str, origin: &str, value: &str) -> anyhow::Result<u64> {
    let value = value.trim();
    match value.parse::<i64>() {
        Ok(n) => Ok(n.max(0) as u64),
        Err(_) => Err(crate::stats::Refusal::at(
            5,
            format!(
                "Config '{name}' from '{origin}' has an invalid value: Invalid value provided for 'integer': '{value}'"
            ),
        )),
    }
}

/// Reads `[defaults]` and `[privilege_escalation]`. Relative paths are relative to the
/// configuration file; `origin` names the file in a refusal, as the reference names it.
fn parse(text: &str, base: &Path, origin: &str) -> anyhow::Result<Config> {
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
                _ => {}
            }
            continue;
        }
        if section != "defaults" {
            continue;
        }
        match key.trim() {
            "inventory" => {
                let first = value.split(',').next().unwrap_or("").trim();
                if !first.is_empty() {
                    config.inventory = Some(base.join(first));
                }
            }
            "timeout" => {
                config.timeout = Duration::from_secs(integer("DEFAULT_TIMEOUT", origin, value)?);
            }
            "remote_user" => {
                let user = value.trim();
                if !user.is_empty() {
                    config.remote_user = Some(user.to_string());
                }
            }
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
            "remote_tmp" => {
                let tmp = value.trim();
                if !tmp.is_empty() {
                    config.remote_tmp = tmp.to_string();
                }
            }
            // A zero reaches the caller as zero, where the single startup check refuses it.
            // A value that is no number at all is the reference's own exit 5 instead.
            "forks" => config.forks = integer("DEFAULT_FORKS", origin, value)? as usize,
            _ => {}
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every sample here is expected to parse; `forks` and `timeout` are the only keys that
    /// can refuse, and the tests that exercise those call `parse` directly.
    fn cfg(text: &str, base: &str) -> Config {
        parse(text, Path::new(base), "ansible.cfg").expect("the sample parses")
    }

    #[test]
    fn defaults_section_is_read_and_paths_are_anchored() {
        let c = cfg(
            "[defaults]\ninventory = hosts.ini, other\ntimeout = 3\n[ssh_connection]\ntimeout = 99\n",
            "/etc/x",
        );
        assert_eq!(c.inventory, Some(PathBuf::from("/etc/x/hosts.ini")));
        assert_eq!(c.timeout, Duration::from_secs(3));
    }

    #[test]
    fn the_connection_keys_are_read_with_ansibles_boolean_spellings() {
        let c = cfg(
            "[defaults]\nremote_user = ops\nprivate_key_file = keys/id\nhost_key_checking = no\nremote_tmp = /var/tmp/v\n",
            "/etc/x",
        );
        assert_eq!(c.remote_user.as_deref(), Some("ops"));
        assert_eq!(c.private_key_file, Some(PathBuf::from("/etc/x/keys/id")));
        assert!(!c.host_key_checking);
        assert_eq!(c.remote_tmp, "/var/tmp/v");
        assert!(
            cfg("[defaults]\nhost_key_checking = True\n", ".").host_key_checking,
            "checking is on by default and True keeps it on"
        );
        assert!(
            cfg("[defaults]\nhost_key_checking = maybe\n", ".").host_key_checking,
            "an unreadable value leaves the safe default alone"
        );
    }

    /// `ANSIBLE_REMOTE_TMP=` used to be taken at face value, and an empty remote path makes
    /// the agent's cache directory impossible to create. The file arm already refused it.
    #[test]
    fn a_blank_remote_tmp_is_refused_wherever_it_comes_from() {
        assert_eq!(
            cfg("[defaults]\nremote_tmp =   \n", ".").remote_tmp,
            DEFAULT_REMOTE_TMP
        );
        let saved_config = std::env::var("ANSIBLE_CONFIG").ok();
        let saved_tmp = std::env::var("ANSIBLE_REMOTE_TMP").ok();
        unsafe {
            std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
            std::env::set_var("ANSIBLE_REMOTE_TMP", "   ");
        }
        assert_eq!(Config::load().unwrap().remote_tmp, DEFAULT_REMOTE_TMP);
        unsafe { std::env::set_var("ANSIBLE_REMOTE_TMP", "/var/tmp/v") };
        assert_eq!(Config::load().unwrap().remote_tmp, "/var/tmp/v");
        unsafe {
            match saved_config {
                Some(v) => std::env::set_var("ANSIBLE_CONFIG", v),
                None => std::env::remove_var("ANSIBLE_CONFIG"),
            }
            match saved_tmp {
                Some(v) => std::env::set_var("ANSIBLE_REMOTE_TMP", v),
                None => std::env::remove_var("ANSIBLE_REMOTE_TMP"),
            }
        }
    }

    /// A zero, and a negative value with it, reach the caller as zero: refusing them is the
    /// startup check's job, and silently falling back to five would run a playbook the
    /// reference refuses outright.
    ///
    /// A value that is no number at all is a different refusal with a different code.
    /// Measured on the development machine against `ansible-core 2.19.12`: `forks = many` in
    /// `ansible.cfg` prints `ERROR: Config 'DEFAULT_FORKS' from '<path>' has an invalid value:
    /// Invalid value provided for 'integer': 'many'` and exits 5, before any play header.
    ///
    /// What would make this red: an unparsable `forks` or `timeout` quietly keeping a default,
    /// or carrying a code other than the 5 the reference gives it.
    #[test]
    fn an_integer_setting_is_read_or_refused_with_the_reference_s_code() {
        assert_eq!(cfg("[defaults]\nforks = 12\n", ".").forks, 12);
        assert_eq!(cfg("[defaults]\nforks = 0\n", ".").forks, 0);
        assert_eq!(
            cfg("[defaults]\nforks = -1\n", ".").forks,
            0,
            "a negative value reaches the startup refusal like a literal zero"
        );
        for (text, name) in [
            ("[defaults]\nforks = many\n", "DEFAULT_FORKS"),
            ("[defaults]\ntimeout = abc\n", "DEFAULT_TIMEOUT"),
        ] {
            let err = parse(text, Path::new("."), "/etc/x/ansible.cfg").unwrap_err();
            assert_eq!(crate::stats::error_code(&err), 5, "{text}");
            let shown = format!("{err:#}");
            assert!(
                shown.contains(&format!(
                    "Config '{name}' from '/etc/x/ansible.cfg' has an invalid value"
                )),
                "{shown}"
            );
            assert!(
                shown.contains("Invalid value provided for 'integer'"),
                "{shown}"
            );
        }
    }

    /// The same two settings from the environment, where the reference names the variable as
    /// the origin instead of a file: `ANSIBLE_TIMEOUT=abc` exits 5 with
    /// `Config 'DEFAULT_TIMEOUT' from 'env: ANSIBLE_TIMEOUT' ...`, measured.
    #[test]
    fn the_environment_arm_refuses_a_non_integer_the_same_way() {
        let saved_config = std::env::var("ANSIBLE_CONFIG").ok();
        let saved_forks = std::env::var("ANSIBLE_FORKS").ok();
        let saved_timeout = std::env::var("ANSIBLE_TIMEOUT").ok();
        unsafe {
            std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
            std::env::set_var("ANSIBLE_FORKS", "-1");
        }
        assert_eq!(Config::load().unwrap().forks, 0);
        unsafe { std::env::set_var("ANSIBLE_FORKS", "3") };
        assert_eq!(Config::load().unwrap().forks, 3);
        unsafe { std::env::set_var("ANSIBLE_FORKS", "abc") };
        let err = Config::load().unwrap_err();
        assert_eq!(crate::stats::error_code(&err), 5);
        assert!(
            format!("{err:#}").contains("from 'env: ANSIBLE_FORKS'"),
            "{err:#}"
        );
        unsafe {
            std::env::set_var("ANSIBLE_FORKS", "3");
            std::env::set_var("ANSIBLE_TIMEOUT", "abc");
        }
        let err = Config::load().unwrap_err();
        assert_eq!(crate::stats::error_code(&err), 5);
        assert!(
            format!("{err:#}").contains("Config 'DEFAULT_TIMEOUT' from 'env: ANSIBLE_TIMEOUT'"),
            "{err:#}"
        );
        unsafe {
            std::env::set_var("ANSIBLE_TIMEOUT", "9");
        }
        assert_eq!(Config::load().unwrap().timeout, Duration::from_secs(9));
        unsafe {
            match saved_config {
                Some(v) => std::env::set_var("ANSIBLE_CONFIG", v),
                None => std::env::remove_var("ANSIBLE_CONFIG"),
            }
            match saved_forks {
                Some(v) => std::env::set_var("ANSIBLE_FORKS", v),
                None => std::env::remove_var("ANSIBLE_FORKS"),
            }
            match saved_timeout {
                Some(v) => std::env::set_var("ANSIBLE_TIMEOUT", v),
                None => std::env::remove_var("ANSIBLE_TIMEOUT"),
            }
        }
    }

    #[test]
    fn the_privilege_escalation_section_is_read_and_stays_out_of_defaults() {
        let c = cfg(
            "[defaults]\nbecome_user = ignored\n[privilege_escalation]\nbecome = yes\nbecome_user = deploy\nbecome_method = sudo\n",
            ".",
        );
        assert!(c.r#become);
        assert_eq!(c.become_user, "deploy");
        assert_eq!(c.become_method, "sudo");
        let c = cfg("[privilege_escalation]\nbecome_method = su\n", ".");
        assert_eq!(
            c.become_method, "su",
            "kept as written so the escalation check can refuse it by name"
        );
        let c = cfg("[privilege_escalation]\nbecome_method =\n", ".");
        assert_eq!(
            c.become_method, DEFAULT_BECOME_METHOD,
            "a blank line asks for nothing and must not refuse every escalated run"
        );
        let c = cfg("[defaults]\nforks = 7\n", ".");
        assert_eq!(
            (c.forks, c.r#become, c.become_user.as_str()),
            (7, false, DEFAULT_BECOME_USER),
            "a file with no escalation section escalates nothing"
        );
    }

    #[test]
    fn missing_values_keep_defaults() {
        assert_eq!(cfg("[defaults]\n", "."), Config::default());
        assert_eq!(cfg("", "."), Config::default());
    }
}
