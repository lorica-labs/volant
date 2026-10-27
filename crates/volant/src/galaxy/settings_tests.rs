// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use super::settings::{Options, ServerSettings, Settings};

fn file() -> Vec<(String, String, String)> {
    [
        ("galaxy", "server_list", "private, public"),
        (
            "galaxy_server.private",
            "url",
            "https://hub.example/api/galaxy/",
        ),
        ("galaxy_server.private", "token", "file-token"),
        (
            "galaxy_server.public",
            "url",
            "https://galaxy.ansible.com/api/",
        ),
        ("galaxy_server.public", "timeout", "5"),
    ]
    .iter()
    .map(|(s, k, v)| ((*s).to_string(), (*k).to_string(), (*v).to_string()))
    .collect()
}

fn server(name: &str, url: &str, token: Option<&str>, timeout: u64) -> ServerSettings {
    ServerSettings {
        name: name.into(),
        url: url.into(),
        token: token.map(Into::into),
        validate_certs: true,
        timeout,
    }
}

fn servers(
    file: &[(String, String, String)],
    env: &[(&str, &str)],
    options: &Options,
) -> Vec<ServerSettings> {
    let env: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let lookup = move |name: &str| env.get(name).cloned();
    Settings::new(file, &lookup).servers(options)
}

#[test]
fn the_server_list_is_read_in_order() {
    assert_eq!(
        servers(&file(), &[], &Options::default()),
        [
            server(
                "private",
                "https://hub.example/api/galaxy/",
                Some("file-token"),
                60
            ),
            server("public", "https://galaxy.ansible.com/api/", None, 5),
        ]
    );
}

#[test]
fn the_environment_comes_before_the_file() {
    let env = [
        ("ANSIBLE_GALAXY_SERVER_LIST", "public"),
        (
            "ANSIBLE_GALAXY_SERVER_PUBLIC_URL",
            "https://mirror.example/api/",
        ),
        ("ANSIBLE_GALAXY_SERVER_TIMEOUT", "30"),
    ];
    assert_eq!(
        servers(&file(), &env, &Options::default()),
        [server("public", "https://mirror.example/api/", None, 5)]
    );
}

#[test]
fn s_names_a_server_or_a_configured_one() {
    let options = Options {
        server: Some("https://other.example/"),
        token: Some("cli-token"),
        ..Options::default()
    };
    assert_eq!(
        servers(&file(), &[], &options),
        [server(
            "cmd_arg",
            "https://other.example/",
            Some("cli-token"),
            60
        )]
    );
    let options = Options {
        server: Some("private"),
        ..Options::default()
    };
    assert_eq!(
        servers(&file(), &[], &options),
        [server(
            "private",
            "https://hub.example/api/galaxy/",
            Some("file-token"),
            60
        )]
    );
}

#[test]
fn the_token_option_beats_the_file() {
    let options = Options {
        token: Some("cli-token"),
        ..Options::default()
    };
    assert_eq!(
        servers(&file(), &[], &options)[0].token.as_deref(),
        Some("cli-token")
    );
}

#[test]
fn without_a_list_the_default_server_is_galaxy() {
    assert_eq!(
        servers(&[], &[], &Options::default()),
        [server("default", "https://galaxy.ansible.com", None, 60)]
    );
    assert_eq!(
        servers(
            &[],
            &[("ANSIBLE_GALAXY_SERVER", "https://own.example/")],
            &Options::default()
        )[0]
        .url,
        "https://own.example/"
    );
}

#[test]
fn ignore_certs_turns_every_check_off() {
    let options = Options {
        ignore_certs: true,
        ..Options::default()
    };
    assert!(
        servers(&file(), &[], &options)
            .iter()
            .all(|s| !s.validate_certs)
    );
    assert!(
        servers(
            &file(),
            &[("ANSIBLE_GALAXY_IGNORE", "yes")],
            &Options::default()
        )
        .iter()
        .all(|s| !s.validate_certs)
    );
}

#[test]
fn the_token_file_and_the_cache_live_under_ansible_home_unless_set() {
    let none = |_: &str| None;
    let defaults = Settings::new(&[], &none);
    assert!(
        defaults.token_path().ends_with(".ansible/galaxy_token")
            || std::env::var_os("ANSIBLE_HOME").is_some()
    );
    assert!(defaults.cache_dir().ends_with("galaxy_cache"));
    let set = |name: &str| (name == "ANSIBLE_GALAXY_TOKEN_PATH").then(|| "/run/token".to_string());
    assert_eq!(
        Settings::new(&[], &set).token_path(),
        std::path::PathBuf::from("/run/token")
    );
    let file = vec![(
        "galaxy".to_string(),
        "cache_dir".to_string(),
        "/var/cache/g".to_string(),
    )];
    assert_eq!(
        Settings::new(&file, &none).cache_dir(),
        std::path::PathBuf::from("/var/cache/g")
    );
}
