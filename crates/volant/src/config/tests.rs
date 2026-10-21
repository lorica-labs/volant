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

/// `show_task_path_on_failure` is a callback option: `y`, `t` and `1` are true in any case, every
/// other word is false, and a variable that is set wins over the file, empty or not.
#[test]
fn the_failure_path_switch_reads_the_spellings_of_the_reference() {
    assert!(!Config::default().show_task_path_on_failure);
    for (word, on) in [
        ("true", true),
        ("TRUE", true),
        ("Yes", true),
        ("on", true),
        ("y", true),
        ("t", true),
        ("1", true),
        ("false", false),
        ("no", false),
        ("0", false),
        ("2", false),
        ("maybe", false),
    ] {
        let text = format!("[defaults]\nshow_task_path_on_failure = {word}\n");
        assert_eq!(cfg(&text, ".").show_task_path_on_failure, on, "{word}");
    }
    let saved_config = std::env::var("ANSIBLE_CONFIG").ok();
    let dir = std::env::temp_dir().join(format!("volant-taskpath-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    let file = dir.join("ansible.cfg");
    std::fs::write(&file, "[defaults]\nshow_task_path_on_failure = true\n").expect("the file");
    unsafe { std::env::set_var("ANSIBLE_CONFIG", &file) };
    assert!(Config::load().unwrap().show_task_path_on_failure);
    for (word, on) in [("no", false), ("", false), ("maybe", false), ("T", true)] {
        unsafe { std::env::set_var("ANSIBLE_SHOW_TASK_PATH_ON_FAILURE", word) };
        assert_eq!(
            Config::load().unwrap().show_task_path_on_failure,
            on,
            "{word:?}"
        );
    }
    unsafe {
        std::env::remove_var("ANSIBLE_SHOW_TASK_PATH_ON_FAILURE");
        match saved_config {
            Some(v) => std::env::set_var("ANSIBLE_CONFIG", v),
            None => std::env::remove_var("ANSIBLE_CONFIG"),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
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

/// A `roles_path` entry comes out absolute with `.` and `..` folded out, as `os.path.abspath`
/// leaves it in ansible-core 2.19.12; the role search and `ansible_search_path` are built from it.
#[rstest::rstest]
#[case::dot_slash("./roles", "/etc/x/roles")]
#[case::bare("roles", "/etc/x/roles")]
#[case::parent("../shared/./roles", "/etc/shared/roles")]
#[case::doubled("a//b/../c", "/etc/x/a/c")]
#[case::absolute("/abs/./r/", "/abs/r")]
fn a_roles_path_entry_is_absolute_and_folded(#[case] entry: &str, #[case] expected: &str) {
    let c = cfg(&format!("[defaults]\nroles_path = {entry}\n"), "/etc/x");
    assert_eq!(c.roles_path, [PathBuf::from(expected)]);
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

/// `[volant] runtime` takes its three words, `auto` when absent, and refuses any other.
#[rstest::rstest]
#[case::absent("[defaults]\nforks = 3\n", Ok(crate::runtime::Mode::Auto))]
#[case::auto("[volant]\nruntime = auto\n", Ok(crate::runtime::Mode::Auto))]
#[case::managed("[volant]\nruntime = managed\n", Ok(crate::runtime::Mode::Managed))]
#[case::system("[volant]\nruntime = system\n", Ok(crate::runtime::Mode::System))]
#[case::other(
    "[volant]\nruntime = venv\n",
    Err("from 'ansible.cfg' has an invalid value: 'venv'; expected one of auto, managed, system")
)]
fn the_volant_section_picks_the_runtime(
    #[case] text: &str,
    #[case] expected: Result<crate::runtime::Mode, &str>,
) {
    match (parse(text, Path::new("."), "ansible.cfg"), expected) {
        (Ok(config), Ok(mode)) => assert_eq!(config.runtime, mode),
        (Err(err), Err(says)) => {
            assert_eq!(crate::stats::error_code(&err), 5, "{err:#}");
            assert!(format!("{err:#}").contains(says), "{err:#}");
        }
        (got, expected) => panic!("got {:?}, expected {expected:?}", got.map(|c| c.runtime)),
    }
}

/// `VOLANT_RUNTIME` wins over the file and is refused the same way.
#[test]
fn volant_runtime_in_the_environment_sets_the_mode() {
    unsafe {
        std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
        std::env::set_var("VOLANT_RUNTIME", "managed");
    }
    assert_eq!(
        Config::load().unwrap().runtime,
        crate::runtime::Mode::Managed
    );
    unsafe { std::env::set_var("VOLANT_RUNTIME", "maybe") };
    let err = Config::load().unwrap_err();
    assert!(
        format!("{err:#}").contains("from 'env: VOLANT_RUNTIME'"),
        "{err:#}"
    );
    unsafe { std::env::remove_var("VOLANT_RUNTIME") };
    assert_eq!(Config::load().unwrap().runtime, crate::runtime::Mode::Auto);
}

/// `find` against `find_ini_config_file` of ansible-core 2.19.12: only the world-write bit of
/// the working directory skips it, and the warning is dropped when `ANSIBLE_CONFIG` named a file.
#[cfg(unix)]
#[rstest::rstest]
#[case::plain_directory(0o755, "file", "none", true, true, "cwd", false)]
#[case::group_writable(0o770, "file", "none", true, true, "cwd", false)]
#[case::world_writable(0o777, "file", "none", true, true, "home", true)]
#[case::world_bit_alone(0o703, "file", "none", true, true, "home", true)]
#[case::sticky_does_not_exempt(0o1777, "file", "none", true, true, "home", true)]
#[case::falls_to_system(0o777, "file", "none", false, true, "system", true)]
#[case::nothing_else(0o777, "file", "none", false, false, "none", true)]
#[case::no_file_no_warning(0o777, "none", "none", true, true, "home", false)]
#[case::dangling_link_is_no_file(0o777, "dangling", "none", true, true, "home", false)]
#[case::link_to_a_file_counts(0o777, "link", "none", true, true, "home", true)]
#[case::link_in_a_plain_directory(0o755, "link", "none", true, true, "cwd", false)]
#[case::env_file_wins_and_is_quiet(0o777, "file", "file", true, true, "env", false)]
#[case::env_file_in_a_plain_directory(0o755, "file", "file", true, true, "env", false)]
fn the_working_directory_is_a_source_unless_world_writable(
    #[case] mode: u32,
    #[case] in_cwd: &str,
    #[case] explicit: &str,
    #[case] home_has: bool,
    #[case] system_has: bool,
    #[case] source: &str,
    #[case] skipped: bool,
) {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let dir = |name: &str| {
        let path = root.path().join(name);
        std::fs::create_dir(&path).unwrap();
        path
    };
    let (cwd, home, other) = (dir("cwd"), dir("home"), dir("other"));
    let system = root.path().join("system.cfg");
    let env_file = other.join("env.cfg");
    std::fs::write(&env_file, "").unwrap();
    match in_cwd {
        "file" => std::fs::write(cwd.join("ansible.cfg"), "").unwrap(),
        "link" => std::os::unix::fs::symlink(&env_file, cwd.join("ansible.cfg")).unwrap(),
        "dangling" => {
            std::os::unix::fs::symlink(other.join("gone"), cwd.join("ansible.cfg")).unwrap();
        }
        _ => {}
    }
    if home_has {
        std::fs::write(home.join(".ansible.cfg"), "").unwrap();
    }
    if system_has {
        std::fs::write(&system, "").unwrap();
    }
    let explicit = match explicit {
        "file" => Some(env_file.clone()),
        _ => None,
    };
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(mode)).unwrap();
    let found = locate::find(
        explicit.as_deref().map(Path::as_os_str),
        &cwd,
        Some(&home),
        &system,
    );
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o755)).unwrap();
    let expected = match source {
        "cwd" => Some(cwd.join("ansible.cfg")),
        "home" => Some(home.join(".ansible.cfg")),
        "system" => Some(system),
        "env" => Some(env_file),
        _ => None,
    };
    assert_eq!(found.path, expected, "the file read");
    assert_eq!(found.skipped, skipped, "the warning");
}

/// A working directory that cannot be read is no source and draws no warning, as in the
/// reference, which catches the `OSError` and goes on.
#[test]
fn a_working_directory_that_is_gone_is_skipped_without_a_warning() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join(".ansible.cfg"), "").unwrap();
    let found = locate::find(
        None,
        &root.path().join("gone"),
        Some(&home),
        &root.path().join("none"),
    );
    assert_eq!(found.path, Some(home.join(".ansible.cfg")));
    assert!(!found.skipped);
}

/// The warning is the reference's, word for word.
#[test]
fn the_world_writable_warning_is_the_references() {
    assert_eq!(
        locate::world_writable_warning(Path::new("/tmp/x")),
        "Ansible is being run in a world writable directory (/tmp/x), ignoring it as an \
         ansible.cfg source. For more information see \
         https://docs.ansible.com/ansible/devel/reference_appendices/config.html#cfg-in-world-writable-dir"
    );
}

/// The mode of a directory decides, and one that cannot be read counts as world-writable, so it
/// is no source.
#[cfg(unix)]
#[rstest::rstest]
#[case::world_writable(0o777, true)]
#[case::sticky(0o1777, true)]
#[case::world_bit_alone(0o702, true)]
#[case::group_writable(0o775, false)]
#[case::plain(0o755, false)]
fn the_mode_of_a_directory_decides_whether_others_can_write_it(
    #[case] mode: u32,
    #[case] expected: bool,
) {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
    assert_eq!(is_world_writable(dir.path()), expected);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(is_world_writable(&dir.path().join("gone")));
}

/// `[volant] host_named_paths` takes its three words, `allow` when absent, and refuses any other
/// with the key, the words and the value.
#[rstest::rstest]
#[case::absent("[defaults]\nforks = 3\n", Ok(HostNamedPaths::Allow))]
#[case::allow("[volant]\nhost_named_paths = allow\n", Ok(HostNamedPaths::Allow))]
#[case::contained(
    "[volant]\nhost_named_paths = contained\n",
    Ok(HostNamedPaths::Contained)
)]
#[case::refuse("[volant]\nhost_named_paths = refuse\n", Ok(HostNamedPaths::Refuse))]
#[case::spaced("[volant]\nhost_named_paths =  refuse \n", Ok(HostNamedPaths::Refuse))]
#[case::other(
    "[volant]\nhost_named_paths = sometimes\n",
    Err(
        "Config 'VOLANT_HOST_NAMED_PATHS' from 'ansible.cfg' has an invalid value: 'sometimes'; expected one of allow, contained, refuse"
    )
)]
#[case::capitalised(
    "[volant]\nhost_named_paths = Allow\n",
    Err("has an invalid value: 'Allow'; expected one of allow, contained, refuse")
)]
#[case::empty(
    "[volant]\nhost_named_paths =\n",
    Err("has an invalid value: ''; expected one of allow, contained, refuse")
)]
#[case::wrong_section("[defaults]\nhost_named_paths = refuse\n", Ok(HostNamedPaths::Allow))]
fn the_volant_section_picks_how_a_host_named_source_is_read(
    #[case] text: &str,
    #[case] expected: Result<HostNamedPaths, &str>,
) {
    match (parse(text, Path::new("."), "ansible.cfg"), expected) {
        (Ok(config), Ok(mode)) => assert_eq!(config.host_named_paths, mode),
        (Err(err), Err(says)) => {
            assert_eq!(crate::stats::error_code(&err), 5, "{err:#}");
            assert!(format!("{err:#}").contains(says), "{err:#}");
        }
        (got, expected) => panic!(
            "got {:?}, expected {expected:?}",
            got.map(|c| c.host_named_paths)
        ),
    }
}

/// `VOLANT_HOST_NAMED_PATHS` wins over the file and is refused the same way.
#[test]
fn volant_host_named_paths_in_the_environment_sets_the_mode() {
    unsafe {
        std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
        std::env::set_var("VOLANT_HOST_NAMED_PATHS", "contained");
    }
    assert_eq!(
        Config::load().unwrap().host_named_paths,
        HostNamedPaths::Contained
    );
    unsafe { std::env::set_var("VOLANT_HOST_NAMED_PATHS", "maybe") };
    let err = Config::load().unwrap_err();
    assert!(
        format!("{err:#}").contains(
            "Config 'VOLANT_HOST_NAMED_PATHS' from 'env: VOLANT_HOST_NAMED_PATHS' has an invalid value: 'maybe'; expected one of allow, contained, refuse"
        ),
        "{err:#}"
    );
    unsafe { std::env::remove_var("VOLANT_HOST_NAMED_PATHS") };
    assert_eq!(
        Config::load().unwrap().host_named_paths,
        HostNamedPaths::Allow
    );
}

/// `[volant] host_named_dest_dirs` is a colon-separated list of absolute directories, unset when
/// absent and empty when blank, and refuses any other entry with the key, the value and the entry.
#[rstest::rstest]
#[case::absent("[defaults]\nforks = 3\n", Ok(None))]
#[case::one("[volant]\nhost_named_dest_dirs = /srv/fetch\n", Ok(Some(vec!["/srv/fetch"])))]
#[case::two(
    "[volant]\nhost_named_dest_dirs = /srv/a : /srv/b/\n",
    Ok(Some(vec!["/srv/a", "/srv/b/"]))
)]
#[case::blank("[volant]\nhost_named_dest_dirs =\n", Ok(Some(vec![])))]
#[case::wrong_section("[defaults]\nhost_named_dest_dirs = /srv/a\n", Ok(None))]
#[case::relative(
    "[volant]\nhost_named_dest_dirs = fetch\n",
    Err(
        "has an invalid value: 'fetch'; expected a colon-separated list of absolute directories (entry 'fetch' is not an absolute path)"
    )
)]
#[case::tilde(
    "[volant]\nhost_named_dest_dirs = /srv/a:~/fetch\n",
    Err("(entry '~/fetch' is not an absolute path)")
)]
#[case::dotdot(
    "[volant]\nhost_named_dest_dirs = /srv/a/../b\n",
    Err("(entry '/srv/a/../b' has a '..' component)")
)]
#[case::empty_entry(
    "[volant]\nhost_named_dest_dirs = /srv/a::/srv/b\n",
    Err("(entry '' is empty)")
)]
#[case::root(
    "[volant]\nhost_named_dest_dirs = /\n",
    Err("(entry '/' is the root directory)")
)]
fn the_volant_section_names_the_directories_a_host_named_dest_may_use(
    #[case] text: &str,
    #[case] expected: Result<Option<Vec<&str>>, &str>,
) {
    match (parse(text, Path::new("/base"), "ansible.cfg"), expected) {
        (Ok(config), Ok(dirs)) => {
            let want: Option<Vec<PathBuf>> =
                dirs.map(|dirs| dirs.into_iter().map(PathBuf::from).collect());
            assert_eq!(config.host_named_dest_dirs, want);
        }
        (Err(err), Err(says)) => {
            assert_eq!(crate::stats::error_code(&err), 5, "{err:#}");
            let said = format!("{err:#}");
            assert!(said.contains("Config 'VOLANT_HOST_NAMED_DEST_DIRS' from 'ansible.cfg'"));
            assert!(said.contains(says), "{said}");
        }
        (got, expected) => panic!(
            "got {:?}, expected {expected:?}",
            got.map(|c| c.host_named_dest_dirs)
        ),
    }
}

/// `VOLANT_HOST_NAMED_DEST_DIRS` replaces the file's list and is refused the same way.
#[test]
fn volant_host_named_dest_dirs_in_the_environment_sets_the_list() {
    unsafe {
        std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
        std::env::set_var("VOLANT_HOST_NAMED_DEST_DIRS", "/srv/a:/srv/b");
    }
    assert_eq!(
        Config::load().unwrap().host_named_dest_dirs,
        Some(vec![PathBuf::from("/srv/a"), PathBuf::from("/srv/b")])
    );
    unsafe { std::env::set_var("VOLANT_HOST_NAMED_DEST_DIRS", "") };
    assert_eq!(
        Config::load().unwrap().host_named_dest_dirs,
        Some(Vec::new())
    );
    unsafe { std::env::set_var("VOLANT_HOST_NAMED_DEST_DIRS", "rel") };
    let err = Config::load().unwrap_err();
    assert!(
        format!("{err:#}").contains(
            "Config 'VOLANT_HOST_NAMED_DEST_DIRS' from 'env: VOLANT_HOST_NAMED_DEST_DIRS' has an invalid value: 'rel'; expected a colon-separated list of absolute directories (entry 'rel' is not an absolute path)"
        ),
        "{err:#}"
    );
    unsafe { std::env::remove_var("VOLANT_HOST_NAMED_DEST_DIRS") };
    assert_eq!(Config::load().unwrap().host_named_dest_dirs, None);
}
