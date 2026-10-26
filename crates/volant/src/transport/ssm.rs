// SPDX-License-Identifier: GPL-3.0-or-later
//! The `amazon.aws.aws_ssm` connection: the agent's link through an AWS Systems Manager Session
//! Manager shell, with the options of amazon.aws 10.1.2.
//!
//! The session is started as the plugin starts it, with boto3 in the controller's Python, and
//! carried by `session-manager-plugin`. It is a terminal that hands the instance's output back as
//! UTF-8, holding back or dropping bytes that are not: the probe and the upload run in that one
//! shell (see [`shell`]), and the agent then talks in base64 lines (`volant-agent --lines`). Files
//! never go through S3, so the plugin's bucket options are read by nobody. The user is the
//! session's own, `ssm-user` on Amazon Linux: the plugin cannot pick one either.

mod shell;

use std::time::Duration;

use serde_json::{Map, Value};

use crate::agent::{AgentLink, AgentSource};

use super::agent_dir::AgentCommands;
use super::bootstrap::{Captured, bootstrap, bootstrap_probe_ran, first_words, run_bootstrap};
use super::choose::remote_tmp_of;
use super::defaults::{BecomeMethod, ConnectError, ConnectionDefaults, Escalation, Secret};
use super::escalation::{RunAs, escalated_command, preamble, preparing_for, settle_form};
use super::names::{
    AWS_SSM_ACCESS_KEY_ID, AWS_SSM_DOCUMENT, AWS_SSM_INSTANCE_ID, AWS_SSM_PLUGIN, AWS_SSM_PROFILE,
    AWS_SSM_REGION, AWS_SSM_SECRET_ACCESS_KEY, AWS_SSM_SESSION_TOKEN, AWS_SSM_TIMEOUT, HOST,
    first_set,
};
use super::reach::Reach;
pub(super) use shell::Shell;

/// The plugin's full name. `aws_ssm` reaches it only through community.aws's redirect.
pub(super) const PLUGIN: &str = "amazon.aws.aws_ssm";

/// Starts the session with boto3 as the plugin does, then runs `session-manager-plugin` and
/// passes it what comes on stdin. The credentials come in the environment and are taken out of
/// it before the plugin starts. When stdin ends, at the end of the link or because the controller
/// was killed, it sends the line that ends the agent's input: the agent stops, and the
/// shell it was started in with `exec` goes with it, which ends the session. A session still up
/// ten seconds later is terminated.
const STARTER: &str = r#"
import json, os, sys
o = json.loads(sys.argv[1])
keys = {arg: os.environ.pop(name, None) for arg, name in (
    ("aws_access_key_id", "VOLANT_SSM_ACCESS_KEY_ID"),
    ("aws_secret_access_key", "VOLANT_SSM_SECRET_ACCESS_KEY"),
    ("aws_session_token", "VOLANT_SSM_SESSION_TOKEN"))}
try:
    import boto3
except ImportError:
    sys.exit("the aws_ssm connection needs boto3 in the controller's Python, %s" % sys.executable)
try:
    client = boto3.session.Session(region_name=o["region"], profile_name=o["profile"], **keys).client("ssm")
    start = {"Target": o["instance"], "Parameters": {}}
    if o["document"]:
        start["DocumentName"] = o["document"]
    answer = client.start_session(**start)
except Exception as failure:
    sys.exit(str(failure))
import subprocess
plugin = subprocess.Popen([o["plugin"], json.dumps(answer), o["region"], "StartSession",
    o["profile"] or "", json.dumps({"Target": o["instance"]}), client.meta.endpoint_url],
    stdin=subprocess.PIPE)
try:
    while True:
        data = os.read(0, 65536)
        if not data:
            break
        plugin.stdin.write(data)
        plugin.stdin.flush()
    # A line cut short ends first; the dot then ends the agent's input.
    plugin.stdin.write(b"\n.\n")
    plugin.stdin.flush()
    plugin.wait(timeout=10)
except Exception:
    pass
if plugin.poll() is None:
    try:
        client.terminate_session(SessionId=answer["SessionId"])
    finally:
        plugin.kill()
sys.exit(plugin.wait())
"#;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SsmTarget {
    /// The host's address, in the `<...>` of the `-vvvv` lines.
    pub name: String,
    pub instance: String,
    pub region: String,
    pub profile: Option<String>,
    pub document: Option<String>,
    /// `session-manager-plugin`, as found.
    pub plugin: String,
    /// Why no link can be made, said when one is asked for.
    pub unavailable: Option<String>,
    /// The access key, its secret and a session token, each with the variable it travels in.
    pub credentials: Vec<(&'static str, Secret)>,
    pub remote_tmp: String,
    /// How long the session has to answer each step: the plugin's `ssm_timeout`.
    pub connect_timeout: Duration,
}

/// The exec target of a host whose connection is `amazon.aws.aws_ssm`.
pub(super) fn target(
    host_name: &str,
    vars: &Map<String, Value>,
    defaults: &ConnectionDefaults,
) -> anyhow::Result<SsmTarget> {
    let text = |names: &[&str], env: &[&str]| {
        first_set(vars, names)
            .and_then(|value| match value {
                Value::String(text) => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            })
            .or_else(|| env.iter().find_map(|name| std::env::var(name).ok()))
            .filter(|text| !text.is_empty())
    };
    let name = text(&HOST, &[]).unwrap_or_else(|| host_name.to_string());
    let instance = text(&AWS_SSM_INSTANCE_ID, &[]).unwrap_or_else(|| name.clone());
    let region = text(&AWS_SSM_REGION, &["AWS_DEFAULT_REGION", "AWS_REGION"])
        .unwrap_or_else(|| "us-east-1".to_string());
    let credentials = [
        (
            &AWS_SSM_ACCESS_KEY_ID,
            "AWS_ACCESS_KEY_ID",
            "VOLANT_SSM_ACCESS_KEY_ID",
        ),
        (
            &AWS_SSM_SECRET_ACCESS_KEY,
            "AWS_SECRET_ACCESS_KEY",
            "VOLANT_SSM_SECRET_ACCESS_KEY",
        ),
        (
            &AWS_SSM_SESSION_TOKEN,
            "AWS_SESSION_TOKEN",
            "VOLANT_SSM_SESSION_TOKEN",
        ),
    ]
    .into_iter()
    .filter_map(|(names, env, carrier)| Some((carrier, Secret::new(text(names, &[env])?))))
    .collect();
    let timeout = first_set(vars, &AWS_SSM_TIMEOUT)
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.trim().parse().ok(),
            _ => None,
        })
        .unwrap_or(60);
    let found = match text(&AWS_SSM_PLUGIN, &["AWS_SESSION_MANAGER_PLUGIN"]) {
        Some(plugin) => Some(plugin),
        None => ["/usr/local/bin/session-manager-plugin"]
            .into_iter()
            .map(std::path::PathBuf::from)
            .chain(
                std::env::var_os("PATH")
                    .map(|path| {
                        std::env::split_paths(&path)
                            .map(|dir| dir.join("session-manager-plugin"))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            )
            .find(|file| file.is_file())
            .map(|file| file.display().to_string()),
    };
    Ok(SsmTarget {
        unavailable: found
            .is_none()
            .then(|| "session-manager-plugin command not found in PATH".to_string()),
        plugin: found.unwrap_or_default(),
        name,
        instance,
        region,
        profile: text(&AWS_SSM_PROFILE, &["AWS_PROFILE"]),
        document: text(&AWS_SSM_DOCUMENT, &[]),
        credentials,
        remote_tmp: remote_tmp_of(host_name, vars, defaults)?,
        connect_timeout: Duration::from_secs(timeout.max(1)),
    })
}

impl SsmTarget {
    /// The local command that starts the session and carries it: the controller's Python with
    /// boto3, which becomes `session-manager-plugin`.
    fn starter(&self, python: &str) -> tokio::process::Command {
        let options = serde_json::json!({
            "instance": self.instance,
            "region": self.region,
            "profile": self.profile,
            "document": self.document,
            "plugin": self.plugin,
        });
        let mut command = tokio::process::Command::new(python);
        command
            .args(["-c", STARTER, &options.to_string()])
            .envs(
                self.credentials
                    .iter()
                    .map(|(name, secret)| (*name, secret.expose())),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Opens a link: the session, then the probe, the upload when the cache is stale and the
    /// agent's start, all in the session's one shell. `sudo` goes ahead of the agent as it does
    /// over `ssh`.
    pub(super) async fn connect(
        &self,
        agents: &AgentSource,
        escalation: Option<&Escalation>,
    ) -> Result<AgentLink, ConnectError> {
        if let Some(why) = &self.unavailable {
            return Err(ConnectError::Unreachable(why.clone()));
        }
        if let Some(e) = escalation
            && e.method == BecomeMethod::Su
        {
            return Err(ConnectError::Become(
                "become_method 'su' is not supported yet on an aws_ssm connection".to_string(),
            ));
        }
        let python = crate::runtime::resolve::controller_python().ok_or_else(|| {
            ConnectError::Unreachable(
                "the aws_ssm connection needs the controller's Python, with boto3".to_string(),
            )
        })?;
        if super::trace::connection_lines() {
            super::trace::note(
                &self.name,
                &format!("ESTABLISH SSM CONNECTION TO: {}", self.instance),
            );
        }
        let child = self
            .starter(&python)
            .spawn()
            .map_err(|e| ConnectError::Unreachable(format!("starting {python}: {e}")))?;
        let shell = Shell::open(child, self.connect_timeout).await?;
        let reach = InBand {
            target: self,
            shell: tokio::sync::Mutex::new(shell),
        };
        let probe_command = reach.probe_command();
        let (escalated, probe) = if let Some(e) = escalation {
            let (form, probe) = settle_form(e, bootstrap_probe_ran, |form| {
                run_bootstrap(&reach, RunAs::Direct(Some((e, form))), &probe_command, None)
            })
            .await?;
            (Some((e, form)), probe)
        } else {
            let probe = run_bootstrap(&reach, RunAs::Direct(None), &probe_command, None).await?;
            (None, probe)
        };
        bootstrap(&reach, agents, RunAs::Direct(escalated), Some(probe))
            .await
            .map_err(|err| preparing_for(err, escalation))?;
        let start = reach.exec_command(" --lines 2>\"$d/agent.stderr\"");
        let start = match escalated {
            None => start,
            Some(_) => format!("exec {}", escalated_command(escalated, &start)),
        };
        reach.trace_exec(std::slice::from_ref(&start));
        let stderr_file = format!("{}/agent.stderr", reach.dir_hint());
        reach
            .shell
            .into_inner()
            .start(&start, preamble(escalated), stderr_file)
            .await
    }
}

/// The target while its session's shell is open: what the bootstrap runs goes to that shell.
struct InBand<'a> {
    target: &'a SsmTarget,
    shell: tokio::sync::Mutex<Shell>,
}

impl Reach for InBand<'_> {
    fn place(&self) -> &str {
        &self.target.name
    }

    fn remote_tmp(&self) -> &str {
        &self.target.remote_tmp
    }

    fn command_line(&self, command: &str, _payload: bool) -> Vec<String> {
        vec![command.to_string()]
    }

    fn environment(&self) -> Result<Vec<(std::ffi::OsString, std::ffi::OsString)>, ConnectError> {
        Ok(Vec::new())
    }

    /// `EXEC: <command>`, as the plugin writes it.
    fn trace_exec(&self, argv: &[String]) {
        super::trace::note(&self.target.name, &format!("EXEC: {}", argv.join(" ")));
    }

    fn trace_result(&self, captured: &Captured) {
        super::trace::result(&self.target.name, captured);
    }

    fn connection_failure(
        &self,
        _captured: &Captured,
        _offered: Option<&str>,
    ) -> Option<ConnectError> {
        None
    }

    fn probe_failure(&self, probe: &Captured) -> Option<ConnectError> {
        Some(ConnectError::Unreachable(format!(
            "the session on {} could not run a command: {}",
            self.target.instance,
            first_words(
                &probe.stderr,
                &first_words(&probe.stdout, "it exited without a message")
            )
        )))
    }

    fn in_band(&self) -> Option<&tokio::sync::Mutex<Shell>> {
        Some(&self.shell)
    }
}

#[cfg(all(test, unix))]
mod tests;
