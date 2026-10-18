// SPDX-License-Identifier: GPL-3.0-or-later
//! An action plugin run as ansible-core's own, in the controller's Python helper.
//!
//! The plugin runs on a thread of its own, since the helper is waited for without the runtime.
//! What it asks of the host comes back here as an [`Ask`] and leaves as one more step over the
//! task's own link: a module as a sub-task, a command as the agent's `raw`, a file as a put. No
//! ask reaches the agent any other way, and the plugin's result goes down the road every remote
//! result takes.
//!
//! The temporary directories it makes are kept track of here. One still on the host when the
//! plugin ends without removing it - the helper died, or the item ran out of time - is removed
//! before the item ends; a directory this action did not make is never removed.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::sync::Notify;
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use super::files::{fits, refuse_host_named};
use super::{Context, Plugin, Step, Sub, is_gone, lost};
use crate::bridge::{Ask, BridgeError, Reply, TmpOp};

/// What the plugin's thread hands over: one more ask, or the end.
enum FromHelper {
    Ask(Ask),
    Done(Result<Value, BridgeError>),
}

/// The step the last ask became, which reads the result handed back.
enum Pending {
    Module,
    Command,
    PutFile,
    MakeTmp,
    RemoveTmp(String),
    /// A directory removed after the plugin ended, whose answer nobody waits for.
    Sweep,
}

/// How long a deadline with no `timeout` keyword is: none the reference would reach either.
const NO_DEADLINE: Duration = Duration::from_secs(60 * 60 * 24 * 365);

pub(super) struct Bridged {
    name: &'static str,
    modules: &'static [&'static str],
    hostvars: Arc<Map<String, Value>>,
    asks: Receiver<FromHelper>,
    replies: Option<Sender<Reply>>,
    ready: Arc<Notify>,
    pending: Option<Pending>,
    /// The temporary directories made for this action and not yet removed.
    tmps: Vec<String>,
    /// Holds the decrypted vaulted `src` for as long as the plugin may read it; the helper has no
    /// secret of its own.
    _vault_src: Option<tempfile::TempDir>,
    /// The item's result, held while the directories left behind are removed.
    ending: Option<TaskResult>,
    timeout: Option<u64>,
    deadline: Instant,
    /// A module ask lost its link, so the next one is asked over a fresh link.
    stale: bool,
    /// The `connect_timeout` argument of `wait_for_connection`, for a fresh link.
    connect_timeout: Duration,
}

/// ansible-core's plugin `name`, which may run `modules` and no other module.
pub(super) fn start(
    name: &'static str,
    modules: &'static [&'static str],
    ctx: Context<'_>,
) -> Box<dyn Plugin> {
    // `find_needle` lets ansible-core's plugins read any controller file a `src` or a script's
    // line names, and put it on the host: a name a managed host chose is never followed.
    for arg in ["src", "_raw_params"] {
        if let Err(msg) = refuse_host_named(ctx.args_untrusted, arg) {
            return super::copy::failing(msg);
        }
    }
    let Some(bridge) = ctx.templar.bridge().cloned() else {
        return super::copy::failing(format!(
            "the action plugin '{name}' needs the controller's Python helper, which this run has not got"
        ));
    };
    let timeout = ctx.timeout.filter(|t| *t > 0);
    let now = Instant::now();
    let deadline = timeout
        .and_then(|t| now.checked_add(Duration::from_secs(t)))
        .or_else(|| now.checked_add(NO_DEADLINE))
        .unwrap_or(now);
    let mut task = Map::new();
    // The terminal is the controller's to give: a plugin forwarding these arguments to `raw` or
    // `command` must not carry a task's own request for one.
    let mut forwarded = ctx.args.clone();
    forwarded.remove("_volant_tty");
    let vault_src = match decrypted_src(name, &ctx) {
        Ok(Some((dir, path))) => {
            forwarded.insert("src".into(), Value::String(path));
            Some(dir)
        }
        Ok(None) => None,
        Err(msg) => return super::copy::failing(msg),
    };
    task.insert("args".into(), Value::Object(forwarded));
    if let Some(environment) = ctx.environment.filter(|e| !e.is_empty()) {
        task.insert("environment".into(), json!(environment));
    }
    // The task's own modes, which the plugin reads off `self._task`: `--check` alone would miss a
    // task's `check_mode: false`, and a plugin that never heard of check mode changes the host.
    task.insert("check_mode".into(), Value::Bool(ctx.check_mode));
    task.insert("diff".into(), Value::Bool(ctx.diff));
    let task = Value::Object(task);
    let vars = ctx.running_vars;
    // The layers in the order a read goes through them: the host's own map last.
    let mut task_vars: Map<String, Value> = (*vars.shared).clone();
    task_vars.extend(vars.facts.iter().map(|(k, v)| (k.clone(), v.clone())));
    task_vars.extend(vars.map.iter().map(|(k, v)| (k.clone(), v.clone())));
    // Each other host's variables are asked for when a plugin reads them.
    task_vars.remove("hostvars");
    // Where the plugin looks for a `src`: the role's root, then the directory of the task's file,
    // as `Task.get_search_path` answers for a task that is in a role.
    let search: Vec<String> = ctx
        .origin
        .role_dir
        .iter()
        .map(std::path::PathBuf::as_path)
        .chain([ctx.origin.file_dir.as_path()])
        .map(|p| p.display().to_string())
        .collect();
    task_vars.insert("_volant_search_path".into(), json!(search));
    let text = |key: &str| task_vars.get(key).and_then(Value::as_str);
    let connection = if ctx.local {
        "local"
    } else {
        text("ansible_connection").unwrap_or("ssh")
    };
    let play_context = json!({
        "connection": connection,
        "remote_addr": text("ansible_host").or_else(|| text("inventory_hostname")),
        "remote_user": text("ansible_user"),
        "check_mode": ctx.check_mode,
        "diff": ctx.diff,
    });
    let task_vars = Value::Object(task_vars);
    let (to_plugin, asks) = channel();
    let (replies, from_plugin) = channel::<Reply>();
    let ready = Arc::new(Notify::new());
    let notify = Arc::clone(&ready);
    std::thread::spawn(move || {
        let mut answer = |ask: Ask| {
            if to_plugin.send(FromHelper::Ask(ask)).is_err() {
                return Reply::Error("the task ended before the host was asked".into());
            }
            notify.notify_one();
            from_plugin
                .recv()
                .unwrap_or_else(|_| Reply::Error("the task ended before the host answered".into()))
        };
        let outcome = bridge.run_action(
            name,
            &task,
            &task_vars,
            &play_context,
            deadline,
            &mut answer,
        );
        let _ = to_plugin.send(FromHelper::Done(outcome));
        notify.notify_one();
    });
    Box::new(Bridged {
        name,
        modules,
        hostvars: Arc::clone(&vars.hostvars),
        asks,
        replies: Some(replies),
        ready,
        pending: None,
        tmps: Vec::new(),
        _vault_src: vault_src,
        ending: None,
        timeout,
        deadline,
        stale: false,
        connect_timeout: Duration::from_secs(
            ctx.args
                .get("connect_timeout")
                .and_then(Value::as_u64)
                .unwrap_or(5),
        ),
    })
}

/// A vaulted controller `src` of `copy` or `template`, decrypted with the run's secrets into a
/// private directory under its own file name, and the path to hand the plugin. `copy` with
/// `decrypt: false` keeps the ciphertext. Secrets never reach the helper.
fn decrypted_src(
    name: &str,
    ctx: &Context<'_>,
) -> Result<Option<(tempfile::TempDir, String)>, String> {
    let args = ctx.args;
    let Some(src) = args.get("src").and_then(Value::as_str) else {
        return Ok(None);
    };
    let kept = args.get("decrypt").and_then(Value::as_bool) == Some(false)
        || args.get("remote_src").and_then(Value::as_bool) == Some(true);
    let folder = match name {
        "copy" if !kept => "files",
        "template" => "templates",
        _ => return Ok(None),
    };
    let searched = super::files::search_paths(ctx.origin, ctx.playbook_dir, folder, src);
    let Some(found) = searched.iter().find(|p| p.is_file()) else {
        return Ok(None);
    };
    let unreadable =
        |err: &dyn std::fmt::Display| format!("could not read src={}: {err}", found.display());
    let bytes = std::fs::read(found).map_err(|e| unreadable(&e))?;
    if !crate::vault::is_encrypted(&bytes) {
        return Ok(None);
    }
    let plain =
        crate::vault::decrypt(&bytes, &crate::vault::run_secrets()).map_err(|e| unreadable(&e))?;
    let dir = tempfile::tempdir().map_err(|e| unreadable(&e))?;
    let path = dir.path().join(found.file_name().unwrap_or_default());
    write_private(&path, &plain).map_err(|e| unreadable(&e))?;
    Ok(Some((dir, path.display().to_string())))
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(bytes)
}

/// `raw`'s arguments for a command line.
fn raw(cmd: String) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("_raw_params".into(), Value::String(cmd));
    args
}

/// A path the controller made and quotes: absolute, one line, and nothing a shell reads inside
/// single quotes.
fn plain_path(path: &str) -> bool {
    path.starts_with('/') && path.len() > 1 && !path.contains(['\'', '\n', '\r', '\0'])
}

/// A `raw` result read as the reply to a command: `{rc, stdout, stderr}`, or why there is none.
fn command_reply(result: &TaskResult) -> Reply {
    let text = |key: &str| {
        result
            .0
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match result.0.get("rc").and_then(Value::as_i64) {
        Some(rc) => {
            Reply::Value(json!({"rc": rc, "stdout": text("stdout"), "stderr": text("stderr")}))
        }
        None => Reply::Error(
            result
                .0
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("the command ended without a return code")
                .to_string(),
        ),
    }
}

/// The reply to anything but a command: its failure's message, or `ok`.
fn failure_or(result: &TaskResult, ok: Value) -> Reply {
    if result.failed() {
        Reply::Error(
            result
                .0
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("failed with no message")
                .to_string(),
        )
    } else {
        Reply::Value(ok)
    }
}

impl Bridged {
    /// The step an ask becomes, or the reply it gets without one.
    fn step_for(&mut self, ask: Ask) -> Result<(Step, Pending), Reply> {
        match ask {
            Ask::Module { name, args, files } => {
                if !files.is_empty() {
                    return Err(Reply::Error(
                        "a module staging controller files is not supported yet in an action plugin"
                            .into(),
                    ));
                }
                let short = short_name(&name);
                // A collection plugin's module is named whole; a builtin one by its short name.
                let module = self
                    .modules
                    .iter()
                    .find(|m| **m == name)
                    .or_else(|| {
                        (!name.contains('.')
                            || name.starts_with("ansible.builtin.")
                            || name.starts_with("ansible.legacy."))
                        .then(|| self.modules.iter().find(|m| **m == short))
                        .flatten()
                    })
                    .ok_or_else(|| {
                    Reply::Error(format!(
                        "the module '{name}' is not supported yet in the action plugin '{}'",
                        self.name
                    ))
                })?;
                let Value::Object(args) = args else {
                    return Err(Reply::Error("the module arguments are not a mapping".into()));
                };
                let sub = Sub {
                    module,
                    args,
                    files: Vec::new(),
                };
                // The ping of a host that is rebooting is the plugin's to retry: a link that
                // goes under it is an error the plugin sees, as the reference's connection
                // failure is, and the next ask is made over a fresh link.
                let step = match (self.name, self.stale) {
                    ("wait_for_connection", true) => Step::Reconnect {
                        timeout: Duration::from_secs(60).min(
                            self.deadline.saturating_duration_since(Instant::now()),
                        ),
                        probe: sub,
                        wait: Duration::ZERO,
                        attempt: Some(self.connect_timeout),
                    },
                    ("wait_for_connection", false) => Step::RunDropping(sub),
                    _ => Step::Run(sub),
                };
                Ok((step, Pending::Module))
            }
            // The command goes over the task's link, which is the escalated one when the task
            // escalates. A `sudoable=False` command (the plugin's housekeeping) also goes over
            // it: there is no unescalated link inside the plugin loop.
            Ask::Command {
                cmd,
                stdin,
                sudoable,
            } => {
                if stdin.is_some() {
                    return Err(Reply::Error(
                        "Connection.exec_command with input is not supported yet".into(),
                    ));
                }
                let mut args = raw(cmd);
                // With no stdin and `sudoable`, the reference sends the command through `ssh -tt`.
                if sudoable {
                    args.insert("_volant_tty".into(), Value::Bool(true));
                }
                Ok((Sub::run("raw", args), Pending::Command))
            }
            Ask::PutFile {
                local,
                remote,
                mode,
            } => {
                let shown = local.display().to_string();
                let len = std::fs::metadata(&local)
                    .map_err(|err| Reply::Error(format!("could not read {shown}: {err}")))?
                    .len();
                fits(&shown, usize::try_from(len).unwrap_or(usize::MAX)).map_err(Reply::Error)?;
                let bytes = std::fs::read(&local)
                    .map_err(|err| Reply::Error(format!("could not read {shown}: {err}")))?;
                Ok((
                    Step::PutFile {
                        bytes,
                        remote,
                        mode,
                    },
                    Pending::PutFile,
                ))
            }
            Ask::FetchFile { .. } => Err(Reply::Error(
                "Connection.fetch_file is not supported yet in an action plugin".into(),
            )),
            Ask::Tmp(TmpOp::Make { remote_user: Some(user) }) => Err(Reply::Error(format!(
                "a temporary directory made as the user {user} is not supported yet"
            ))),
            Ask::Tmp(TmpOp::Make { remote_user: None }) => Ok((
                Sub::run(
                    "raw",
                    raw("umask 77 && mkdir -p ~/.ansible/tmp && mktemp -d ~/.ansible/tmp/ansible-tmp-XXXXXXXXXX".into()),
                ),
                Pending::MakeTmp,
            )),
            Ask::Tmp(TmpOp::Remove { path }) => {
                if !self.tmps.contains(&path) {
                    return Err(Reply::Error(format!(
                        "the temporary directory {path} was not made for this action"
                    )));
                }
                Ok((Sub::run("raw", raw(format!("rm -f -r '{path}'"))), Pending::RemoveTmp(path)))
            }
            Ask::HostVar { host } => Err(Reply::Value(
                self.hostvars.get(&host).cloned().unwrap_or(Value::Null),
            )),
        }
    }

    /// The reply a sub-task's result makes to the ask it answered.
    fn reply_for(&mut self, pending: Pending, last: Option<TaskResult>) -> Reply {
        let Some(result) = last else {
            return Reply::Error("the controller lost the host's answer".into());
        };
        match pending {
            Pending::Module if self.name == "wait_for_connection" && is_gone(&result) => {
                self.stale = true;
                failure_or(&result, Value::Null)
            }
            Pending::Module => {
                self.stale = false;
                Reply::Value(Value::Object(result.0))
            }
            Pending::Command => command_reply(&result),
            Pending::PutFile => failure_or(&result, Value::Null),
            Pending::MakeTmp => match command_reply(&result) {
                Reply::Value(value) if value["rc"] == 0 => {
                    let path = value["stdout"]
                        .as_str()
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if !plain_path(&path) {
                        return Reply::Error(format!(
                            "failed to resolve remote temporary directory: the host answered {path:?}"
                        ));
                    }
                    self.tmps.push(path.clone());
                    Reply::Value(Value::String(path))
                }
                Reply::Value(value) => Reply::Error(format!(
                    "failed to create a remote temporary directory: {}",
                    value["stderr"].as_str().unwrap_or_default().trim()
                )),
                error @ Reply::Error(_) => error,
            },
            Pending::RemoveTmp(path) => match command_reply(&result) {
                Reply::Value(value) if value["rc"] == 0 => {
                    self.tmps.retain(|p| *p != path);
                    Reply::Value(Value::Null)
                }
                Reply::Value(value) => Reply::Error(
                    value["stderr"]
                        .as_str()
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                ),
                error @ Reply::Error(_) => error,
            },
            Pending::Sweep => Reply::Value(Value::Null),
        }
    }

    /// The item's result from what the helper ended with.
    fn result_of(&self, outcome: Result<Value, BridgeError>) -> TaskResult {
        match outcome {
            Ok(Value::Object(map)) => {
                let raised_skip = map.contains_key(crate::executor::RAISED_SKIP);
                let mut result = TaskResult(map);
                crate::executor::drop_internal_keys(&mut result);
                if raised_skip {
                    result
                        .0
                        .insert(crate::executor::RAISED_SKIP.into(), Value::Bool(true));
                }
                result
            }
            Ok(other) => TaskResult::failed_with(format!(
                "the action plugin '{}' answered {other} rather than a result",
                self.name
            )),
            Err(BridgeError::Helper(_)) if Instant::now() >= self.deadline => {
                TaskResult::timed_out(self.timeout.unwrap_or(0))
            }
            Err(err) => TaskResult::failed_with(err.to_string()),
        }
    }

    /// One removal of a directory left behind, as a sub-task.
    fn sweep(path: &str) -> Sub {
        Sub {
            module: "raw",
            args: raw(format!("rm -f -r '{path}'")),
            files: Vec::new(),
        }
    }
}

impl Plugin for Bridged {
    fn next(&mut self, last: Option<TaskResult>) -> Step {
        if let Some(pending) = self.pending.take() {
            let reply = self.reply_for(pending, last);
            if self.ending.is_none()
                && let Some(replies) = &self.replies
            {
                // A helper gone already reads nothing more; its end comes through `asks`.
                let _ = replies.send(reply);
            }
        }
        if self.ending.is_some() {
            if let Some(path) = self.tmps.pop() {
                self.pending = Some(Pending::Sweep);
                return Step::Run(Self::sweep(&path));
            }
            return Step::Done(self.ending.take().unwrap_or_else(|| lost(self.name)));
        }
        loop {
            match self.asks.try_recv() {
                Ok(FromHelper::Ask(ask)) => match self.step_for(ask) {
                    Ok((step, pending)) => {
                        self.pending = Some(pending);
                        return step;
                    }
                    Err(reply) => {
                        if let Some(replies) = &self.replies {
                            let _ = replies.send(reply);
                        }
                    }
                },
                Ok(FromHelper::Done(outcome)) => {
                    let result = self.result_of(outcome);
                    self.replies = None;
                    // A directory still on the host is the plugin's that never got to remove it.
                    if let Some(path) = self.tmps.pop() {
                        self.ending = Some(result);
                        self.pending = Some(Pending::Sweep);
                        return Step::Run(Self::sweep(&path));
                    }
                    return Step::Done(result);
                }
                Err(TryRecvError::Empty) => return Step::Wait(Arc::clone(&self.ready)),
                Err(TryRecvError::Disconnected) => {
                    return Step::Done(TaskResult::failed_with(format!(
                        "the action plugin '{}' ended without a result",
                        self.name
                    )));
                }
            }
        }
    }

    fn abandoned(&mut self) -> Vec<Sub> {
        self.tmps.drain(..).map(|path| Self::sweep(&path)).collect()
    }
}

#[cfg(test)]
mod tests;
