// SPDX-License-Identifier: GPL-3.0-or-later
//! The variable names behind each connection setting, in the order they win.
//!
//! ansible-core 2.19.12 lists the variables of a setting in its plugin and lets the last one
//! it finds beat the others, so the legacy `ansible_ssh_*` names beat the plain `ansible_*`
//! ones when a host sets both. Each list here is that order reversed: the first name set is the
//! one that applies. A variable that holds no value (null) counts as not set.

use serde_json::Value;

use crate::vars::{HostSettings, host_setting};

pub(super) const HOST: [&str; 2] = ["ansible_ssh_host", "ansible_host"];
pub(super) const PORT: [&str; 2] = ["ansible_ssh_port", "ansible_port"];
pub(super) const USER: [&str; 2] = ["ansible_ssh_user", "ansible_user"];
pub(super) const PRIVATE_KEY: [&str; 2] =
    ["ansible_ssh_private_key_file", "ansible_private_key_file"];
pub(super) const TIMEOUT: [&str; 2] = ["ansible_ssh_timeout", "ansible_timeout"];
pub(super) const PASSWORD: [&str; 3] = [
    "ansible_ssh_password",
    "ansible_ssh_pass",
    "ansible_password",
];
pub(super) const EXECUTABLE: [&str; 1] = ["ansible_ssh_executable"];

/// community.docker 4.8.5's `community.docker.docker`.
pub(super) const DOCKER_HOST: [&str; 2] = ["ansible_docker_host", "ansible_host"];
pub(super) const DOCKER_USER: [&str; 2] = ["ansible_docker_user", "ansible_user"];
pub(super) const DOCKER_EXTRA_ARGS: [&str; 1] = ["ansible_docker_extra_args"];
pub(super) const DOCKER_EXTRA_ENV: [&str; 1] = ["ansible_docker_extra_env"];
pub(super) const DOCKER_PRIVILEGED: [&str; 1] = ["ansible_docker_privileged"];
pub(super) const DOCKER_WORKING_DIR: [&str; 1] = ["ansible_docker_working_dir"];
pub(super) const DOCKER_TIMEOUT: [&str; 1] = ["ansible_docker_timeout"];

/// containers.podman 1.18.0's `containers.podman.podman`, as its source reads them: the container
/// and the user are the play context's, so the documented `ansible_podman_host` is not one.
pub(super) const PODMAN_HOST: [&str; 1] = ["ansible_host"];
pub(super) const PODMAN_USER: [&str; 1] = ["ansible_user"];
pub(super) const PODMAN_EXECUTABLE: [&str; 1] = ["ansible_podman_executable"];
pub(super) const PODMAN_EXTRA_ARGS: [&str; 1] = ["ansible_podman_extra_args"];

/// community.general 11.4.2's `community.general.chroot`; its directory is the address, as
/// podman's container is.
pub(super) const CHROOT_EXE: [&str; 1] = ["ansible_chroot_exe"];
pub(super) const CHROOT_DISABLE_ROOT_CHECK: [&str; 1] = ["ansible_chroot_disable_root_check"];
pub(super) const CHROOT_EXECUTABLE: [&str; 1] = ["ansible_executable"];

/// community.general 11.4.2's `community.general.lxd` and `community.general.incus`, in the order
/// host, remote, project, executable.
pub(super) const LXD: [&str; 4] = [
    "ansible_lxd_host",
    "ansible_lxd_remote",
    "ansible_lxd_project",
    "ansible_lxd_executable",
];
pub(super) const INCUS: [&str; 4] = [
    "ansible_incus_host",
    "ansible_incus_remote",
    "ansible_incus_project",
    "ansible_incus_executable",
];

/// kubernetes.core 6.2.0's `kubernetes.core.kubectl`.
pub(super) const KUBECTL_POD: [&str; 1] = ["ansible_kubectl_pod"];
pub(super) const KUBECTL_CONTAINER: [&str; 1] = ["ansible_kubectl_container"];
pub(super) const KUBECTL_NAMESPACE: [&str; 1] = ["ansible_kubectl_namespace"];
pub(super) const KUBECTL_EXTRA_ARGS: [&str; 1] = ["ansible_kubectl_extra_args"];
pub(super) const KUBECTL_LOCAL_ENV_VARS: [&str; 1] = ["ansible_kubectl_local_env_vars"];
pub(super) const KUBECTL_KUBECONFIG: [&str; 2] =
    ["ansible_kubectl_config", "ansible_kubectl_kubeconfig"];
pub(super) const KUBECTL_CONTEXT: [&str; 1] = ["ansible_kubectl_context"];
pub(super) const KUBECTL_SERVER: [&str; 2] = ["ansible_kubectl_server", "ansible_kubectl_host"];
pub(super) const KUBECTL_USERNAME: [&str; 2] = ["ansible_kubectl_user", "ansible_kubectl_username"];
pub(super) const KUBECTL_PASSWORD: [&str; 1] = ["ansible_kubectl_password"];
pub(super) const KUBECTL_TOKEN: [&str; 2] = ["ansible_kubectl_api_key", "ansible_kubectl_token"];
pub(super) const KUBECTL_CLIENT_CERT: [&str; 2] =
    ["ansible_kubectl_client_cert", "ansible_kubectl_cert_file"];
pub(super) const KUBECTL_CLIENT_KEY: [&str; 2] =
    ["ansible_kubectl_client_key", "ansible_kubectl_key_file"];
pub(super) const KUBECTL_CA_CERT: [&str; 2] =
    ["ansible_kubectl_ca_cert", "ansible_kubectl_ssl_ca_cert"];
pub(super) const KUBECTL_VALIDATE_CERTS: [&str; 2] = [
    "ansible_kubectl_validate_certs",
    "ansible_kubectl_verify_ssl",
];

/// Every name above, for the scan that checks a managed host cannot set any of them through
/// the facts it returns.
#[cfg(test)]
pub(crate) fn all() -> impl Iterator<Item = &'static str> {
    HOST.into_iter()
        .chain(PORT)
        .chain(USER)
        .chain(PRIVATE_KEY)
        .chain(TIMEOUT)
        .chain(PASSWORD)
        .chain(EXECUTABLE)
        .chain(DOCKER_HOST)
        .chain(DOCKER_USER)
        .chain(DOCKER_EXTRA_ARGS)
        .chain(DOCKER_EXTRA_ENV)
        .chain(DOCKER_PRIVILEGED)
        .chain(DOCKER_WORKING_DIR)
        .chain(DOCKER_TIMEOUT)
        .chain(PODMAN_EXECUTABLE)
        .chain(PODMAN_EXTRA_ARGS)
        .chain(CHROOT_EXE)
        .chain(CHROOT_DISABLE_ROOT_CHECK)
        .chain(CHROOT_EXECUTABLE)
        .chain(LXD)
        .chain(INCUS)
        .chain(KUBECTL_POD)
        .chain(KUBECTL_CONTAINER)
        .chain(KUBECTL_NAMESPACE)
        .chain(KUBECTL_EXTRA_ARGS)
        .chain(KUBECTL_LOCAL_ENV_VARS)
        .chain(KUBECTL_KUBECONFIG)
        .chain(KUBECTL_CONTEXT)
        .chain(KUBECTL_SERVER)
        .chain(KUBECTL_USERNAME)
        .chain(KUBECTL_PASSWORD)
        .chain(KUBECTL_TOKEN)
        .chain(KUBECTL_CLIENT_CERT)
        .chain(KUBECTL_CLIENT_KEY)
        .chain(KUBECTL_CA_CERT)
        .chain(KUBECTL_VALIDATE_CERTS)
}

/// The value of the first of `names` the host sets.
pub(crate) fn first_set<'a, V: HostSettings + ?Sized>(
    vars: &'a V,
    names: &[&str],
) -> Option<&'a Value> {
    names
        .iter()
        .find_map(|name| host_setting(vars, name).filter(|value| !value.is_null()))
}

/// A password as text. A number is its own digits, which is how an inventory line
/// `ansible_password=1234` reaches here; any other shape is no password.
pub(crate) fn secret_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}
