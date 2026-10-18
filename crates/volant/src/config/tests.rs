// SPDX-License-Identifier: GPL-3.0-or-later
use super::paths::home;
use super::*;

/// Every sample here is expected to parse; `forks` and `timeout` are the only keys that
/// can refuse, and the tests that exercise those call `parse` directly.
fn cfg(text: &str, base: &str) -> Config {
    parse(text, Path::new(base), "ansible.cfg").expect("the sample parses")
}

/// Cross-host batching is off unless a `[volant] batching` line asks for it, and that section
/// leaves `[defaults]` undisturbed.
#[test]
fn the_volant_section_carries_the_batching_switch() {
    assert!(!Config::default().batching, "the strict linear by default");
    assert!(!cfg("[defaults]\nforks = 3\n", ".").batching);
    assert!(cfg("[volant]\nbatching = true\n", ".").batching);
    assert!(cfg("[volant]\nbatching = 1\n", ".").batching);
    assert!(!cfg("[volant]\nbatching = no\n", ".").batching);
    assert!(!cfg("[volant]\nbatching = 0\n", ".").batching);
    assert!(!cfg("[volant]\nbatching = maybe\n", ".").batching);
    // An unknown key in the section is passed over, and the section does not swallow the
    // one behind it.
    let c = cfg(
        "[volant]\nbatching = yes\nsomething_else = 4\n[defaults]\nforks = 9\n",
        ".",
    );
    assert!(c.batching);
    assert_eq!(c.forks, 9);
}

/// Natives are on unless the section or the environment turns them off, and a value that is no
/// switch is refused rather than read as one.
#[test]
fn the_volant_section_carries_the_native_modules_switch() {
    assert!(
        cfg(
            "[defaults]
forks = 3
",
            "."
        )
        .native_modules
    );
    assert!(
        !cfg(
            "[volant]
native_modules = false
",
            "."
        )
        .native_modules
    );
    assert!(
        !cfg(
            "[volant]
native_modules = 0
",
            "."
        )
        .native_modules
    );
    assert!(
        cfg(
            "[volant]
native_modules = yes
",
            "."
        )
        .native_modules
    );
    let err = parse(
        "[volant]
native_modules = maybe
",
        Path::new("."),
        "/etc/x/ansible.cfg",
    )
    .unwrap_err();
    assert_eq!(crate::stats::error_code(&err), 5, "{err:#}");
    assert_eq!(
        format!("{err:#}"),
        "Config 'VOLANT_NATIVE_MODULES' from '/etc/x/ansible.cfg' has an invalid value: Invalid value provided for 'boolean': 'maybe'"
    );

    let saved_config = std::env::var("ANSIBLE_CONFIG").ok();
    unsafe {
        std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
        std::env::set_var("VOLANT_NATIVE_MODULES", "0");
    }
    assert!(!Config::load().unwrap().native_modules);
    unsafe { std::env::set_var("VOLANT_NATIVE_MODULES", "maybe") };
    let err = Config::load().unwrap_err();
    assert!(
        format!("{err:#}").contains("from 'env: VOLANT_NATIVE_MODULES'"),
        "{err:#}"
    );
    unsafe { std::env::remove_var("VOLANT_NATIVE_MODULES") };
    assert!(Config::load().unwrap().native_modules);
    unsafe {
        match saved_config {
            Some(v) => std::env::set_var("ANSIBLE_CONFIG", v),
            None => std::env::remove_var("ANSIBLE_CONFIG"),
        }
    }
}

#[test]
fn the_vault_settings_come_from_the_file_and_the_environment() {
    let c = cfg(
        "[defaults]
vault_password_file = pw
vault_identity_list = a@x, b@prompt
",
        "/etc/x",
    );
    assert_eq!(c.vault_password_file, Some(PathBuf::from("/etc/x/pw")));
    assert_eq!(c.vault_identity_list, ["a@x", "b@prompt"]);
    if let Some(home) = home() {
        let c = cfg(
            "[defaults]
vault_password_file = ~/.vault_pass
",
            "/etc/x",
        );
        assert_eq!(c.vault_password_file, Some(home.join(".vault_pass")));
    }

    let saved_config = std::env::var("ANSIBLE_CONFIG").ok();
    unsafe {
        std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
        std::env::set_var("ANSIBLE_VAULT_PASSWORD_FILE", "/v/pw");
        std::env::set_var("ANSIBLE_VAULT_IDENTITY_LIST", "x@pw1, y@prompt");
    }
    let c = Config::load().unwrap();
    assert_eq!(c.vault_password_file, Some(PathBuf::from("/v/pw")));
    assert_eq!(c.vault_identity_list, ["x@pw1", "y@prompt"]);
    unsafe {
        std::env::remove_var("ANSIBLE_VAULT_PASSWORD_FILE");
        std::env::remove_var("ANSIBLE_VAULT_IDENTITY_LIST");
        match saved_config {
            Some(v) => std::env::set_var("ANSIBLE_CONFIG", v),
            None => std::env::remove_var("ANSIBLE_CONFIG"),
        }
    }
}

/// Connection sharing is on unless the section turns it off; the two keys of the section
/// are read independently of each other.
#[test]
fn the_volant_section_can_turn_connection_sharing_off() {
    assert!(cfg("[defaults]\nforks = 3\n", ".").ssh_control_master);
    assert!(!cfg("[volant]\nssh_control_master = 0\n", ".").ssh_control_master);
    assert!(!cfg("[volant]\nssh_control_master = false\n", ".").ssh_control_master);
    let c = cfg("[volant]\nbatching = yes\nssh_control_master = no\n", ".");
    assert!(c.batching && !c.ssh_control_master);
    assert!(cfg("[volant]\nssh_control_master = maybe\n", ".").ssh_control_master);

    let saved_config = std::env::var("ANSIBLE_CONFIG").ok();
    unsafe {
        std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
        std::env::set_var("VOLANT_SSH_CONTROL_MASTER", "0");
    }
    assert!(!Config::load().unwrap().ssh_control_master);
    unsafe { std::env::remove_var("VOLANT_SSH_CONTROL_MASTER") };
    assert!(Config::load().unwrap().ssh_control_master);
    unsafe {
        match saved_config {
            Some(v) => std::env::set_var("ANSIBLE_CONFIG", v),
            None => std::env::remove_var("ANSIBLE_CONFIG"),
        }
    }
}

#[test]
fn defaults_section_is_read_and_paths_are_anchored() {
    let c = cfg(
        "[defaults]\ninventory = hosts.ini, other\ntimeout = 3\n[ssh_connection]\ntimeout = 99\n",
        "/etc/x",
    );
    assert_eq!(
        c.inventory,
        [
            PathBuf::from("/etc/x/hosts.ini"),
            PathBuf::from("/etc/x/other")
        ]
    );
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

/// A zero or negative setting reaches the caller as zero, and a non-number is refused with the
/// reference's code 5.
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

/// A negative connection timeout is refused rather than clamped, from the file and from the
/// environment alike.
#[test]
fn a_negative_timeout_is_refused_rather_than_clamped_to_no_limit() {
    assert_eq!(
        cfg("[defaults]\ntimeout = 7\n", ".").timeout,
        Duration::from_secs(7)
    );
    let err = parse(
        "[defaults]\ntimeout = -1\n",
        Path::new("."),
        "/etc/x/ansible.cfg",
    )
    .unwrap_err();
    assert_eq!(crate::stats::error_code(&err), 2, "{err:#}");
    let shown = format!("{err:#}");
    assert!(shown.contains("must be >= 0, got -1"), "{shown}");
    assert!(shown.contains("/etc/x/ansible.cfg"), "{shown}");
}

/// The same two settings from the environment are refused the same way, naming the variable as
/// the origin.
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
    let c = cfg(
        "[privilege_escalation]\nbecome_exe = /usr/bin/sudo\nbecome_flags = -E\n",
        ".",
    );
    assert_eq!(c.become_exe.as_deref(), Some("/usr/bin/sudo"));
    assert_eq!(c.become_flags.as_deref(), Some("-E"));
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

#[test]
fn the_inventory_section_feeds_the_plugin_chain() {
    let c = cfg(
        "[inventory]\nenable_plugins = ini, host_list\nignore_extensions = .ini\n\
             ignore_patterns = ^sub, 30\nunparsed_is_failed = yes\nany_unparsed_is_failed = 1\n",
        ".",
    );
    let p = &c.inventory_plugins;
    assert_eq!(p.enabled, ["ini", "host_list"]);
    assert_eq!(p.ignore_extensions, [".ini"]);
    assert_eq!(p.ignore_patterns, ["^sub", "30"]);
    assert!(p.unparsed_is_failed && p.any_unparsed_is_failed);
    let base = std::env::temp_dir();
    let bad = parse("[inventory]\nunparsed_is_failed = maybe\n", &base, "x.cfg");
    assert!(bad.is_err(), "a bad boolean is refused");
}
