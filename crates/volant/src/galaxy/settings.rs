// SPDX-License-Identifier: GPL-3.0-or-later
//! The Galaxy settings of ansible-core 2.19.12: `GALAXY_SERVER`, `GALAXY_SERVER_LIST`,
//! `GALAXY_SERVER_TIMEOUT`, `GALAXY_TOKEN_PATH`, `GALAXY_CACHE_DIR`, `GALAXY_IGNORE_CERTS` and the
//! keys of each `[galaxy_server.<name>]`, read from the environment first, then `ansible.cfg`, then
//! the defaults, and the servers a run talks to (`GalaxyCLI.run`).

use std::path::PathBuf;

/// One server a run may ask, in the order it asks them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    /// `cmd_arg` for `-s`, `default` for `GALAXY_SERVER`, or the name in the server list.
    pub name: String,
    pub url: String,
    pub token: Option<String>,
    pub validate_certs: bool,
    pub timeout: u64,
}

/// What the command line gives.
#[derive(Debug, Default, Clone)]
pub struct Options<'a> {
    pub server: Option<&'a str>,
    pub token: Option<&'a str>,
    pub ignore_certs: bool,
    pub timeout: Option<u64>,
}

/// The settings a run reads, from `[galaxy]` and `[galaxy_server.*]` (`Config::galaxy`) and the
/// environment.
pub struct Settings<'a> {
    file: &'a [(String, String, String)],
    env: &'a dyn Fn(&str) -> Option<String>,
}

impl<'a> Settings<'a> {
    pub fn new(
        file: &'a [(String, String, String)],
        env: &'a dyn Fn(&str) -> Option<String>,
    ) -> Self {
        Self { file, env }
    }

    /// The environment variable, else the key of the section.
    fn get(&self, variable: &str, section: &str, key: &str) -> Option<String> {
        (self.env)(variable).or_else(|| {
            self.file
                .iter()
                .rev()
                .find(|(s, k, _)| s == section && k == key)
                .map(|(_, _, v)| v.clone())
        })
    }

    fn galaxy(&self, key: &str) -> Option<String> {
        self.get(
            &format!("ANSIBLE_GALAXY_{}", key.to_uppercase()),
            "galaxy",
            key,
        )
    }

    pub fn server_timeout(&self) -> u64 {
        self.galaxy("server_timeout")
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(60)
    }

    fn ignore_certs(&self) -> bool {
        self.get("ANSIBLE_GALAXY_IGNORE", "galaxy", "ignore_certs")
            .and_then(|v| crate::yaml::bool_from_str(&v))
            .unwrap_or(false)
    }

    /// `GALAXY_TOKEN_PATH`, `<ANSIBLE_HOME>/galaxy_token` by default.
    pub fn token_path(&self) -> PathBuf {
        self.galaxy("token_path")
            .map_or_else(|| ansible_home().join("galaxy_token"), PathBuf::from)
    }

    /// `GALAXY_CACHE_DIR`, `<ANSIBLE_HOME>/galaxy_cache` by default.
    pub fn cache_dir(&self) -> PathBuf {
        self.galaxy("cache_dir")
            .map_or_else(|| ansible_home().join("galaxy_cache"), PathBuf::from)
    }

    /// The server a requirement's `source` URL names when no configured one is it, built as
    /// the reference builds it: no token, the run's certificate checks, `GalaxyAPI`'s 60 seconds.
    pub fn explicit(&self, options: &Options, name: &str, url: &str) -> ServerSettings {
        ServerSettings {
            name: format!("explicit_requirement_{name}"),
            url: url.to_string(),
            token: None,
            validate_certs: !(options.ignore_certs || self.ignore_certs()),
            timeout: 60,
        }
    }

    /// The servers to ask, in order: the one `-s` names (a configured one when the name
    /// matches), else the server list, else `GALAXY_SERVER` as `default`.
    pub fn servers(&self, options: &Options) -> Vec<ServerSettings> {
        let validate = !(options.ignore_certs || self.ignore_certs());
        let timeout = options.timeout.unwrap_or_else(|| self.server_timeout());
        let listed: Vec<ServerSettings> = self
            .galaxy("server_list")
            .map(|list| crate::config::tag_list(&list))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|name| {
                let section = format!("galaxy_server.{name}");
                let key = |k: &str| {
                    self.get(
                        &format!(
                            "ANSIBLE_GALAXY_SERVER_{}_{}",
                            name.to_uppercase(),
                            k.to_uppercase()
                        ),
                        &section,
                        k,
                    )
                };
                let url = key("url")?;
                Some(ServerSettings {
                    token: options.token.map(str::to_string).or_else(|| key("token")),
                    validate_certs: validate
                        && key("validate_certs")
                            .and_then(|v| crate::yaml::bool_from_str(&v))
                            .unwrap_or(true),
                    timeout: options
                        .timeout
                        .or_else(|| key("timeout").and_then(|t| t.trim().parse().ok()))
                        .unwrap_or(timeout),
                    name,
                    url,
                })
            })
            .collect();
        let named = |name: &str, url: String| ServerSettings {
            name: name.into(),
            url,
            token: options.token.map(str::to_string),
            validate_certs: validate,
            timeout,
        };
        if let Some(server) = options.server {
            return vec![
                listed
                    .iter()
                    .find(|s| s.name == server)
                    .cloned()
                    .unwrap_or_else(|| named("cmd_arg", server.to_string())),
            ];
        }
        if listed.is_empty() {
            let url = self
                .galaxy("server")
                .unwrap_or_else(|| "https://galaxy.ansible.com".into());
            return vec![named("default", url)];
        }
        listed
    }
}

/// `ANSIBLE_HOME`, `~/.ansible` by default.
pub fn ansible_home() -> PathBuf {
    std::env::var_os("ANSIBLE_HOME").map_or_else(
        || {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".ansible")
        },
        PathBuf::from,
    )
}
