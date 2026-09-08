// SPDX-License-Identifier: GPL-3.0-or-later
//! The handful of `ansible.cfg` settings this release reads, found where ansible-playbook
//! looks for them, with the matching environment variables taking precedence.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::executor::DEFAULT_CONNECT_TIMEOUT;

/// Ansible's own default for `remote_tmp`, where the agent is cached on a host.
pub const DEFAULT_REMOTE_TMP: &str = "~/.ansible/tmp";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub inventory: Option<PathBuf>,
    pub timeout: Duration,
    pub remote_user: Option<String>,
    pub private_key_file: Option<PathBuf>,
    pub host_key_checking: bool,
    pub remote_tmp: String,
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
        }
    }
}

impl Config {
    pub fn load() -> Config {
        let mut config = match locate() {
            Some(path) => std::fs::read_to_string(&path)
                .map(|text| parse(&text, path.parent().unwrap_or(Path::new("."))))
                .unwrap_or_default(),
            None => Config::default(),
        };
        if let Ok(inv) = std::env::var("ANSIBLE_INVENTORY") {
            config.inventory = Some(PathBuf::from(inv));
        }
        if let Ok(t) = std::env::var("ANSIBLE_TIMEOUT")
            && let Ok(secs) = t.parse::<u64>()
        {
            config.timeout = Duration::from_secs(secs);
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
        config
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

/// Reads `[defaults]` only. Relative paths are relative to the configuration file.
fn parse(text: &str, base: &Path) -> Config {
    let mut config = Config::default();
    let mut in_defaults = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_defaults = section.trim() == "defaults";
            continue;
        }
        if !in_defaults {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "inventory" => {
                let first = value.split(',').next().unwrap_or("").trim();
                if !first.is_empty() {
                    config.inventory = Some(base.join(first));
                }
            }
            "timeout" => {
                if let Ok(secs) = value.trim().parse::<u64>() {
                    config.timeout = Duration::from_secs(secs);
                }
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
            _ => {}
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_section_is_read_and_paths_are_anchored() {
        let c = parse(
            "[defaults]\ninventory = hosts.ini, other\ntimeout = 3\n[ssh_connection]\ntimeout = 99\n",
            Path::new("/etc/x"),
        );
        assert_eq!(c.inventory, Some(PathBuf::from("/etc/x/hosts.ini")));
        assert_eq!(c.timeout, Duration::from_secs(3));
    }

    #[test]
    fn the_connection_keys_are_read_with_ansibles_boolean_spellings() {
        let c = parse(
            "[defaults]\nremote_user = ops\nprivate_key_file = keys/id\nhost_key_checking = no\nremote_tmp = /var/tmp/v\n",
            Path::new("/etc/x"),
        );
        assert_eq!(c.remote_user.as_deref(), Some("ops"));
        assert_eq!(c.private_key_file, Some(PathBuf::from("/etc/x/keys/id")));
        assert!(!c.host_key_checking);
        assert_eq!(c.remote_tmp, "/var/tmp/v");
        assert!(
            parse("[defaults]\nhost_key_checking = True\n", Path::new(".")).host_key_checking,
            "checking is on by default and True keeps it on"
        );
        assert!(
            parse("[defaults]\nhost_key_checking = maybe\n", Path::new(".")).host_key_checking,
            "an unreadable value leaves the safe default alone"
        );
    }

    /// `ANSIBLE_REMOTE_TMP=` used to be taken at face value, and an empty remote path makes
    /// the agent's cache directory impossible to create. The file arm already refused it.
    #[test]
    fn a_blank_remote_tmp_is_refused_wherever_it_comes_from() {
        assert_eq!(
            parse("[defaults]\nremote_tmp =   \n", Path::new(".")).remote_tmp,
            DEFAULT_REMOTE_TMP
        );
        let saved_config = std::env::var("ANSIBLE_CONFIG").ok();
        let saved_tmp = std::env::var("ANSIBLE_REMOTE_TMP").ok();
        unsafe {
            std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
            std::env::set_var("ANSIBLE_REMOTE_TMP", "   ");
        }
        assert_eq!(Config::load().remote_tmp, DEFAULT_REMOTE_TMP);
        unsafe { std::env::set_var("ANSIBLE_REMOTE_TMP", "/var/tmp/v") };
        assert_eq!(Config::load().remote_tmp, "/var/tmp/v");
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

    #[test]
    fn missing_values_keep_defaults() {
        assert_eq!(parse("[defaults]\n", Path::new(".")), Config::default());
        assert_eq!(parse("", Path::new(".")), Config::default());
    }
}
