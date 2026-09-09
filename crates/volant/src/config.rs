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
    /// A file that is there and cannot be read stops the run. It used to fall back to the
    /// defaults, which quietly threw away `inventory`, `forks` and the whole
    /// `[privilege_escalation]` section: the run then targeted the implicit localhost, did
    /// nothing and exited 0. The reference does exactly that - measured, an `ansible.cfg` at
    /// mode 000 leaves it warning only that no inventory was parsed, and it exits 0 - and this
    /// is a deliberate divergence from it: half a configuration is not a configuration, and
    /// this release already refuses a `forks` value it cannot parse for the same reason.
    pub fn load() -> anyhow::Result<Config> {
        let mut config = match locate() {
            Some(path) => {
                let text = std::fs::read_to_string(&path).map_err(|err| {
                    crate::stats::Refusal::at(2, format!("reading {}: {err}", path.display()))
                })?;
                parse(&text, path.parent().unwrap_or(Path::new(".")))
            }
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
        // A zero is kept rather than dropped: the reference refuses it from every source, and
        // the single check at startup is what says so. An unparsable value is refused the same
        // way: measured on the development machine, `ANSIBLE_FORKS=-1` refuses with that exact
        // message (exit 2) and `ANSIBLE_FORKS=abc` refuses too, through its own config-loading
        // error (exit 5) -- neither is tolerated, so silently keeping whatever `forks` already
        // held would run a playbook the reference never would.
        if let Ok(n) = std::env::var("ANSIBLE_FORKS") {
            config.forks = n.trim().parse::<usize>().unwrap_or(0);
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

/// Reads `[defaults]` and `[privilege_escalation]`. Relative paths are relative to the
/// configuration file.
fn parse(text: &str, base: &Path) -> Config {
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
            // A zero and an unparsable value both reach the caller as zero, where the single
            // startup check refuses them. The reference refuses `forks = abc` in the file too,
            // before any play runs, so keeping the default of five here would run a playbook
            // with a fork count the operator never wrote.
            "forks" => config.forks = value.trim().parse().unwrap_or(0),
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

    /// A zero reaches the caller untouched: refusing it is the startup check's job, and
    /// silently falling back to five would run a playbook the reference refuses outright.
    ///
    /// An unparsable value goes the same way. Measured on the development machine against
    /// `ansible-core 2.19.12`: `forks = abc` in `ansible.cfg` refuses with
    /// `ERROR: Config 'DEFAULT_FORKS' from '<path>' has an invalid value` and exit 5, before
    /// any play header. Keeping five here would have run the playbook it refuses.
    #[test]
    fn forks_is_read_and_a_zero_or_an_unparsable_value_is_passed_through() {
        assert_eq!(parse("[defaults]\nforks = 12\n", Path::new(".")).forks, 12);
        assert_eq!(parse("[defaults]\nforks = 0\n", Path::new(".")).forks, 0);
        assert_eq!(
            parse("[defaults]\nforks = many\n", Path::new(".")).forks,
            0,
            "an unparsable value must reach the startup refusal, not fall back to the default"
        );
        assert_eq!(
            parse("[defaults]\nforks = -1\n", Path::new(".")).forks,
            0,
            "a negative value is no more usable than a word"
        );
    }

    /// Measured on the development machine against `ansible-core 2.19.12`: neither
    /// `ANSIBLE_FORKS=-1` nor `ANSIBLE_FORKS=abc` is tolerated (see architecture.md for the
    /// exact output), so an unparsable value here refuses at the same startup check as a
    /// literal zero instead of silently keeping the default of five.
    #[test]
    fn an_unparsable_ansible_forks_is_passed_through_like_a_zero() {
        let saved_config = std::env::var("ANSIBLE_CONFIG").ok();
        let saved_forks = std::env::var("ANSIBLE_FORKS").ok();
        unsafe {
            std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
            std::env::set_var("ANSIBLE_FORKS", "abc");
        }
        assert_eq!(Config::load().unwrap().forks, 0);
        unsafe { std::env::set_var("ANSIBLE_FORKS", "-1") };
        assert_eq!(Config::load().unwrap().forks, 0);
        unsafe { std::env::set_var("ANSIBLE_FORKS", "3") };
        assert_eq!(Config::load().unwrap().forks, 3);
        unsafe {
            match saved_config {
                Some(v) => std::env::set_var("ANSIBLE_CONFIG", v),
                None => std::env::remove_var("ANSIBLE_CONFIG"),
            }
            match saved_forks {
                Some(v) => std::env::set_var("ANSIBLE_FORKS", v),
                None => std::env::remove_var("ANSIBLE_FORKS"),
            }
        }
    }

    #[test]
    fn the_privilege_escalation_section_is_read_and_stays_out_of_defaults() {
        let c = parse(
            "[defaults]\nbecome_user = ignored\n[privilege_escalation]\nbecome = yes\nbecome_user = deploy\nbecome_method = sudo\n",
            Path::new("."),
        );
        assert!(c.r#become);
        assert_eq!(c.become_user, "deploy");
        assert_eq!(c.become_method, "sudo");
        let c = parse(
            "[privilege_escalation]\nbecome_method = su\n",
            Path::new("."),
        );
        assert_eq!(
            c.become_method, "su",
            "kept as written so the escalation check can refuse it by name"
        );
        let c = parse("[privilege_escalation]\nbecome_method =\n", Path::new("."));
        assert_eq!(
            c.become_method, DEFAULT_BECOME_METHOD,
            "a blank line asks for nothing and must not refuse every escalated run"
        );
        let c = parse("[defaults]\nforks = 7\n", Path::new("."));
        assert_eq!(
            (c.forks, c.r#become, c.become_user.as_str()),
            (7, false, DEFAULT_BECOME_USER),
            "a file with no escalation section escalates nothing"
        );
    }

    #[test]
    fn missing_values_keep_defaults() {
        assert_eq!(parse("[defaults]\n", Path::new(".")), Config::default());
        assert_eq!(parse("", Path::new(".")), Config::default());
    }
}
