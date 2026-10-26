// SPDX-License-Identifier: GPL-3.0-or-later
//! The `kubernetes.core.kubectl` connection: the agent's link through `kubectl exec -i`, for a
//! container of a pod, with the options of kubernetes.core 6.2.0 read as its plugin reads them.
//!
//! The plugin builds its flags in the order its documentation lists the options and passes each
//! value as Python prints it, so `validate_certs: true` gives `--insecure-skip-tls-verify True`.
//! It cannot choose the user: commands run as the container's own. `kubectl cp` needs `tar` in
//! the container, so a pod with no shell gets no `copy` strategy.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use tempfile::TempPath;

use serde_json::{Map, Value};

use super::choose::remote_tmp_of;
use super::defaults::ConnectionDefaults;
use super::exec::{ExecTarget, KeptFile, Lines};
use super::names::{
    CHROOT_EXECUTABLE, HOST, KUBECTL_CA_CERT, KUBECTL_CLIENT_CERT, KUBECTL_CLIENT_KEY,
    KUBECTL_CONTAINER, KUBECTL_CONTEXT, KUBECTL_EXTRA_ARGS, KUBECTL_KUBECONFIG,
    KUBECTL_LOCAL_ENV_VARS, KUBECTL_NAMESPACE, KUBECTL_PASSWORD, KUBECTL_POD, KUBECTL_SERVER,
    KUBECTL_TOKEN, KUBECTL_USERNAME, KUBECTL_VALIDATE_CERTS, first_set,
};

/// The plugin's full name, which a short `kubectl` resolves to.
pub(super) const PLUGIN: &str = "kubernetes.core.kubectl";

/// The options after `--kubeconfig` that become a flag and its value, in the plugin's order:
/// the variables, then the environment variables, each list in the order it wins.
const FLAGS: [(&[&str], &[&str], &str); 9] = [
    (&KUBECTL_CONTEXT, &["K8S_AUTH_CONTEXT"], "--context"),
    (
        &KUBECTL_SERVER,
        &["K8S_AUTH_SERVER", "K8S_AUTH_HOST"],
        "--server",
    ),
    (&KUBECTL_USERNAME, &["K8S_AUTH_USERNAME"], "--username"),
    (&KUBECTL_PASSWORD, &["K8S_AUTH_PASSWORD"], "--password"),
    (
        &KUBECTL_TOKEN,
        &["K8S_AUTH_API_KEY", "K8S_AUTH_TOKEN"],
        "--token",
    ),
    (
        &KUBECTL_CLIENT_CERT,
        &["K8S_AUTH_CERT_FILE"],
        "--client-certificate",
    ),
    (&KUBECTL_CLIENT_KEY, &["K8S_AUTH_KEY_FILE"], "--client-key"),
    (
        &KUBECTL_CA_CERT,
        &["K8S_AUTH_SSL_CA_CERT"],
        "--certificate-authority",
    ),
    (
        &KUBECTL_VALIDATE_CERTS,
        &["K8S_AUTH_VERIFY_SSL"],
        "--insecure-skip-tls-verify",
    ),
];

/// The exec target of a host whose connection is `kubernetes.core.kubectl`.
pub(super) fn target(
    host_name: &str,
    vars: &Map<String, Value>,
    defaults: &ConnectionDefaults,
) -> anyhow::Result<ExecTarget> {
    let option = |names: &[&str], env: &[&str]| {
        first_set(vars, names).cloned().or_else(|| {
            env.iter()
                .find_map(|name| std::env::var(name).ok())
                .map(Value::String)
        })
    };
    let set = |names: &[&str], env: &[&str]| {
        option(names, env)
            .filter(crate::template::truthy)
            .map(|value| word(&value))
    };
    let found = super::lxd::program("kubectl", std::env::var_os("PATH").as_deref());
    let mut prefix = vec![
        found
            .as_ref()
            .map_or_else(|| "kubectl".to_string(), |path| path.display().to_string()),
    ];
    if let Some(namespace) = set(&KUBECTL_NAMESPACE, &["K8S_AUTH_NAMESPACE"]) {
        prefix.extend(["-n".to_string(), namespace]);
    }
    let mut kept = None;
    match option(&KUBECTL_KUBECONFIG, &["K8S_AUTH_KUBECONFIG"]) {
        Some(Value::Object(config)) => {
            let path = written(&config)?;
            prefix.extend(["--kubeconfig".to_string(), path.display().to_string()]);
            kept = Some(KeptFile(path));
        }
        Some(Value::String(path)) if path.is_empty() => {}
        Some(other) => prefix.extend(["--kubeconfig".to_string(), word(&other)]),
        None => {}
    }
    let mut secrets = Vec::new();
    for (names, env, flag) in FLAGS {
        if let Some(value) = set(names, env) {
            if matches!(flag, "--password" | "--token") {
                secrets.push(value.clone());
            }
            prefix.extend([flag.to_string(), value]);
        }
    }
    if let Some(extra) = set(&KUBECTL_EXTRA_ARGS, &["K8S_AUTH_EXTRA_ARGS"]) {
        prefix.extend(extra.split(' ').map(str::to_string));
    }
    let address = first_set(vars, &HOST).map_or_else(|| host_name.to_string(), word);
    let pod = set(&KUBECTL_POD, &["K8S_AUTH_POD"]).unwrap_or_else(|| address.clone());
    prefix.extend(["exec".to_string(), "-i".to_string(), pod]);
    if let Some(container) = set(&KUBECTL_CONTAINER, &["K8S_AUTH_CONTAINER"]) {
        prefix.extend(["-c".to_string(), container]);
    }
    prefix.push("--".to_string());
    let env = match first_set(vars, &KUBECTL_LOCAL_ENV_VARS) {
        Some(Value::Object(map)) => map.clone(),
        Some(Value::String(text)) if !text.is_empty() => {
            serde_json::from_str(text).map_err(|err| {
                anyhow::anyhow!(
                    "host '{host_name}': kubectl_local_env_vars is no JSON mapping: {err}"
                )
            })?
        }
        _ => Map::new(),
    };
    let shell = first_set(vars, &CHROOT_EXECUTABLE)
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| defaults.plugins.chroot.executable.clone())
        .unwrap_or_else(|| "/bin/sh".to_string());
    Ok(ExecTarget {
        unavailable: found
            .is_none()
            .then(|| "kubectl command not found in PATH".to_string()),
        quiet_stderr: false,
        failures: Vec::new(),
        env: env
            .iter()
            .map(|(key, value)| (key.clone(), word(value)))
            .collect(),
        secrets,
        kept,
        info: None,
        copy: None,
        user_probe: None,
        prefix,
        name: address,
        kind: "kubectl".into(),
        lines: Lines::NoUser,
        shell,
        user: None,
        remote_tmp: remote_tmp_of(host_name, vars, defaults)?,
        connect_timeout: defaults.connect_timeout,
    })
}

/// The file of each kubeconfig mapping in use, by its text, so that every task's target for one
/// mapping names one file and so shares one link. The file goes with the last target naming it.
static WRITTEN: Mutex<BTreeMap<String, Weak<TempPath>>> = Mutex::new(BTreeMap::new());

/// The private file holding `config`, written unless a live target already names one.
fn written(config: &Map<String, Value>) -> anyhow::Result<Arc<TempPath>> {
    let text = serde_json::to_string(config)?;
    let mut written = WRITTEN.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(path) = written.get(&text).and_then(Weak::upgrade) {
        return Ok(path);
    }
    let mut file = tempfile::Builder::new()
        .prefix("volant-kubeconfig-")
        .tempfile()?;
    file.write_all(text.as_bytes())?;
    let path = Arc::new(file.into_temp_path());
    written.insert(text, Arc::downgrade(&path));
    Ok(path)
}

/// A value as the plugin hands it to `kubectl`: Python's `str` of it.
fn word(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

#[cfg(all(test, unix))]
mod tests;
