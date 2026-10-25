// SPDX-License-Identifier: GPL-3.0-or-later
//! Which connection plugin a name stands for, as the controller's ansible-core resolves it.
//!
//! ansible-core 2.19.12 routes a short name through `ansible_builtin_runtime.yml`: with
//! `community.docker` installed, `docker` is `community.docker.docker`, and without it the task
//! fails with `the connection plugin 'docker' was not found`. Asking the controller's own loader
//! keeps that answer in step with whatever collections are installed. Each name is asked once per
//! run; `local` and `ssh` never need asking.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

/// What the controller's ansible-core made of one connection name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A plugin: its full name, and the names the loader went through to reach it, the asked one
    /// first.
    Found { fqcn: String, hops: Vec<String> },
    /// No plugin by that name, or a routing into a collection nobody installed, with the warning
    /// the loader gave for it.
    Missing { warning: Option<String> },
    /// A name the loader refuses outright, such as one a `runtime.yml` tombstone removed.
    Unusable(String),
}

/// Anything that can ask the controller's ansible-core about a connection name.
pub trait ResolveConnection: Send + Sync {
    /// # Errors
    /// Why the question could not be asked: no ansible-core, a helper that died.
    fn resolve_connection(&self, name: &str) -> anyhow::Result<Resolution>;
}

/// The run's connection names and what each resolved to. Empty when the run has no way to ask,
/// and then a name other than `local` and `ssh` is answered from what this release knows.
#[derive(Clone, Default)]
pub struct ConnectionNames(Option<Arc<Names>>);

struct Names {
    resolver: Arc<dyn ResolveConnection>,
    known: Mutex<HashMap<String, Resolution>>,
    /// The `(host, name)` pairs whose redirect lines were given out.
    said: Mutex<HashSet<(String, String)>>,
    /// Redirect lines waiting for their host's driver to print them.
    lines: Mutex<HashMap<String, Vec<String>>>,
}

impl ConnectionNames {
    pub fn through(resolver: Arc<dyn ResolveConnection>) -> ConnectionNames {
        ConnectionNames(Some(Arc::new(Names {
            resolver,
            known: Mutex::default(),
            said: Mutex::default(),
            lines: Mutex::default(),
        })))
    }

    /// What `name` resolves to for `host`, `None` when nothing can be asked. The first answer for
    /// a host keeps the reference's `-vvv` redirect lines for [`ConnectionNames::take_lines`].
    pub(super) fn resolve(&self, host: &str, name: &str) -> anyhow::Result<Option<Resolution>> {
        let Some(names) = &self.0 else {
            return Ok(None);
        };
        let known = names
            .known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned();
        let resolution = if let Some(resolution) = known {
            resolution
        } else {
            let resolution = names.resolver.resolve_connection(name)?;
            names
                .known
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(name.to_string(), resolution.clone());
            resolution
        };
        if let Resolution::Found { hops, .. } = &resolution
            && hops.len() > 2
            && names
                .said
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert((host.to_string(), name.to_string()))
        {
            names
                .lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(host.to_string())
                .or_default()
                .extend(redirect_lines(hops));
        }
        Ok(Some(resolution))
    }

    /// The redirect lines resolving a name for `host` left, for its driver to print under the
    /// task that resolved it; each pair of host and name gives them once a run.
    pub fn take_lines(&self, host: &str) -> Vec<String> {
        self.0.as_ref().map_or_else(Vec::new, |names| {
            names
                .lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(host)
                .unwrap_or_default()
        })
    }
}

/// The reference's `-vvv` lines for a connection name its loader routed elsewhere, one per hop
/// past the qualified name: `docker` goes through `ansible.builtin.docker` to
/// `community.docker.docker` and prints one line.
fn redirect_lines(hops: &[String]) -> Vec<String> {
    hops.windows(2)
        .skip(1)
        .map(|pair| format!("redirecting (type: connection) {} to {}", pair[0], pair[1]))
        .collect()
}

impl PartialEq for ConnectionNames {
    fn eq(&self, other: &ConnectionNames) -> bool {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for ConnectionNames {}

impl std::fmt::Debug for ConnectionNames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "ConnectionNames(asked)"
        } else {
            "ConnectionNames(none)"
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rstest::rstest;
    use serde_json::{Map, Value};

    use super::{ConnectionNames, Resolution, ResolveConnection, redirect_lines};
    use crate::transport::{ConnectionNotFound, Transport};

    /// Answers as ansible-core 2.19.12 does with community.general installed and community.docker
    /// absent, and counts the questions.
    struct Controller(AtomicUsize);

    impl ResolveConnection for Controller {
        fn resolve_connection(&self, name: &str) -> anyhow::Result<Resolution> {
            self.0.fetch_add(1, Ordering::Relaxed);
            let found = |fqcn: &str, hops: &[&str]| Resolution::Found {
                fqcn: fqcn.into(),
                hops: hops.iter().map(ToString::to_string).collect(),
            };
            Ok(match name {
                "local" | "ssh" => panic!("'{name}' is never asked"),
                "chroot" => found(
                    "community.general.chroot",
                    &["chroot", "ansible.builtin.chroot", "community.general.chroot"],
                ),
                "ansible.builtin.ssh" => found("ansible.builtin.ssh", &["ansible.builtin.ssh"]),
                "winrm" => found("ansible.builtin.winrm", &["winrm"]),
                "gone" => Resolution::Unusable("AnsiblePluginRemovedError: removed".into()),
                "broken" => anyhow::bail!("the helper died"),
                "docker" => Resolution::Missing {
                    warning: Some("Error loading plugin 'community.docker.docker': No module named 'ansible_collections.community.docker'".into()),
                },
                _ => Resolution::Missing { warning: None },
            })
        }
    }

    fn transport(names: &ConnectionNames, connection: &str) -> anyhow::Result<Transport> {
        let vars: Map<String, Value> = [("ansible_connection".into(), Value::from(connection))]
            .into_iter()
            .collect();
        let defaults = crate::transport::ConnectionDefaults {
            names: names.clone(),
            ..crate::testing::defaults()
        };
        Transport::for_vars("h", &vars, &defaults)
    }

    fn asked() -> (ConnectionNames, Arc<Controller>) {
        let controller = Arc::new(Controller(AtomicUsize::new(0)));
        (
            ConnectionNames::through(Arc::clone(&controller) as _),
            controller,
        )
    }

    #[rstest]
    #[case("local", "local")]
    #[case("ssh", "ssh")]
    #[case("ansible.builtin.ssh", "ssh")]
    fn a_name_for_a_plugin_volant_drives_runs_it(#[case] name: &str, #[case] want: &str) {
        let (names, _) = asked();
        let got = match transport(&names, name).unwrap() {
            Transport::Local => "local",
            Transport::Ssh(_) => "ssh",
            Transport::Exec(_) => "exec",
        };
        assert_eq!(got, want);
    }

    #[rstest]
    #[case(
        "chroot",
        "host 'h': connection 'community.general.chroot' is not supported"
    )]
    #[case(
        "winrm",
        "host 'h': connection 'ansible.builtin.winrm' is not supported"
    )]
    #[case(
        "gone",
        "host 'h': connection 'gone' cannot be used: AnsiblePluginRemovedError: removed"
    )]
    fn a_plugin_volant_does_not_drive_is_refused_by_its_full_name(
        #[case] name: &str,
        #[case] want: &str,
    ) {
        let (names, _) = asked();
        assert_eq!(transport(&names, name).unwrap_err().to_string(), want);
    }

    #[rstest]
    #[case(
        "docker",
        Some(
            "Error loading plugin 'community.docker.docker': No module named 'ansible_collections.community.docker'"
        )
    )]
    #[case("paramiko", None)]
    fn a_name_the_controller_finds_nothing_for_fails_the_task(
        #[case] name: &str,
        #[case] warning: Option<&str>,
    ) {
        let (names, _) = asked();
        let err = transport(&names, name).unwrap_err();
        let missing = err.downcast_ref::<ConnectionNotFound>().unwrap();
        assert_eq!(
            (missing.0.as_str(), missing.warning().as_deref()),
            (name, warning)
        );
    }

    #[rstest]
    #[case(&["ssh"], &[])]
    #[case(&["community.general.incus"], &[])]
    #[case(
        &["docker", "ansible.builtin.docker", "community.docker.docker"],
        &["redirecting (type: connection) ansible.builtin.docker to community.docker.docker"]
    )]
    #[case(
        &["aws_ssm", "ansible.builtin.aws_ssm", "community.aws.aws_ssm", "amazon.aws.aws_ssm"],
        &[
            "redirecting (type: connection) ansible.builtin.aws_ssm to community.aws.aws_ssm",
            "redirecting (type: connection) community.aws.aws_ssm to amazon.aws.aws_ssm",
        ]
    )]
    fn a_routed_connection_prints_one_line_per_hop(#[case] hops: &[&str], #[case] want: &[&str]) {
        let hops: Vec<String> = hops.iter().map(ToString::to_string).collect();
        assert_eq!(redirect_lines(&hops), want);
    }

    #[test]
    fn the_redirect_lines_come_once_per_host_and_name() {
        let (names, _) = asked();
        let line =
            "redirecting (type: connection) ansible.builtin.chroot to community.general.chroot";
        let _ = transport(&names, "chroot");
        let _ = transport(&names, "chroot");
        assert_eq!(names.take_lines("h"), [line]);
        let _ = transport(&names, "chroot");
        assert!(names.take_lines("h").is_empty());
        assert!(names.take_lines("other").is_empty());
    }

    #[test]
    fn each_name_is_asked_once_per_run() {
        let (names, controller) = asked();
        for _ in 0..3 {
            let _ = transport(&names, "chroot");
            let _ = transport(&names, "docker");
        }
        assert_eq!(controller.0.load(Ordering::Relaxed), 2);
    }

    /// With no ansible-core to ask, or one that could not answer, a name is read as before.
    #[rstest]
    #[case(
        ConnectionNames::default(),
        "paramiko",
        Some("host 'h': connection 'paramiko' is not supported")
    )]
    #[case(
        ConnectionNames::default(),
        "community.docker.docker",
        Some("host 'h': connection 'community.docker.docker' is not supported")
    )]
    #[case(ConnectionNames::default(), "docker", None)]
    #[case(asked().0, "broken", None)]
    fn without_an_answer_a_name_is_read_from_what_this_release_knows(
        #[case] names: ConnectionNames,
        #[case] name: &str,
        #[case] refused: Option<&str>,
    ) {
        let err = transport(&names, name).unwrap_err();
        match refused {
            Some(text) => assert_eq!(err.to_string(), text),
            None => assert!(
                err.downcast_ref::<ConnectionNotFound>().is_some(),
                "{err:#}"
            ),
        }
    }

    #[test]
    fn ansible_builtin_local_runs_with_nothing_to_ask() {
        assert!(matches!(
            transport(&ConnectionNames::default(), "ansible.builtin.local").unwrap(),
            Transport::Local
        ));
    }
}
