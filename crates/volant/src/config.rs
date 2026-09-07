// SPDX-License-Identifier: GPL-3.0-or-later
//! The handful of `ansible.cfg` settings this release reads, found where ansible-playbook
//! looks for them, with the matching environment variables taking precedence.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::executor::DEFAULT_CONNECT_TIMEOUT;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub inventory: Option<PathBuf>,
    pub timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            inventory: None,
            timeout: DEFAULT_CONNECT_TIMEOUT,
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
    fn missing_values_keep_defaults() {
        assert_eq!(parse("[defaults]\n", Path::new(".")), Config::default());
        assert_eq!(parse("", Path::new(".")), Config::default());
    }
}
