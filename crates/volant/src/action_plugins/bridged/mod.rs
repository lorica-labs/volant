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

use super::files::refuse_host_named;
use super::{Context, Plugin, Step, Sub, lost};
use crate::bridge::{Ask, BridgeError, Reply};

mod asks;
mod helpers;

use helpers::decrypted_src;

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
#[expect(clippy::too_many_lines)]
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
        Ok(Some((dir, key, value))) => {
            forwarded.insert(key.into(), Value::String(value));
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
    // Where the plugin looks for a `src`: the role's root, the directory of the task's file, then
    // the playbook's directory, as `Task.get_search_path` answers for a task that is in a role.
    // The parent roles of a dependency chain are not walked.
    let search: Vec<String> = ctx
        .origin
        .role_dir
        .iter()
        .map(std::path::PathBuf::as_path)
        .chain([ctx.origin.file_dir.as_path(), ctx.playbook_dir])
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
