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

use super::files::{ansible_bool, check_host_named, contained_refusal};
use super::{Context, Plugin, Step, Sub, lost};
use crate::bridge::{Ask, BridgeError, Reply};
use crate::config::HostNamedPaths;

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
    /// The hosts holding a value a managed host sent, as the task's host and the host the module
    /// runs on see them.
    untrusted_hosts: [Arc<std::collections::BTreeSet<String>>; 2],
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

/// The fields of ansible-core's `PlayContext` a plugin reads off the link its module runs over.
/// `running` is that host's own variables. What a plugin that runs `rsync -e ssh` from the
/// controller reads is here too: the key, and the escalation it asks the other end for. A field
/// the link has no value for is left out, as the reference leaves it unset.
fn play_context_of(ctx: &Context<'_>, running: &Map<String, Value>) -> Value {
    let text = |key: &str| running.get(key).and_then(Value::as_str);
    let connection = if ctx.local {
        "local"
    } else {
        text("ansible_connection").unwrap_or("ssh")
    };
    let mut play_context = json!({
        "connection": connection,
        "remote_addr": text("ansible_host").or_else(|| text("inventory_hostname")),
        "remote_user": text("ansible_user").or(ctx.link.remote_user.as_deref()),
        "check_mode": ctx.check_mode,
        "diff": ctx.diff,
    });
    if let Some(key) = &ctx.link.private_key {
        play_context["private_key_file"] = json!(key);
    }
    if let Some((method, user)) = &ctx.link.escalation {
        play_context["become"] = json!(true);
        play_context["become_method"] = json!(method);
        play_context["become_user"] = json!(user);
    }
    play_context
}

/// One host's variables as a plugin reads them: the layers in the order a read goes through
/// them, the host's own map last. Each other host's variables are asked for when a plugin reads
/// them, so `hostvars` is left out.
fn flat_vars(vars: &crate::vars::HostVars) -> Map<String, Value> {
    let mut flat: Map<String, Value> = (*vars.shared).clone();
    flat.extend(vars.facts.iter().map(|(k, v)| (k.clone(), v.clone())));
    flat.extend(
        vars.own_variables()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    // Each other host's variables are asked for when a plugin reads them.
    flat.remove("hostvars");
    flat
}

/// The controller files the arguments of ansible-core's plugin `name` name, held to
/// `host_named_paths` when a managed host named them.
///
/// Read off the plugins of ansible-core 2.19.12, the arguments that take a controller path are
/// `src` of `copy`, `template`, `unarchive`, `assemble` and `uri` (and of a collection's plugin
/// that reads one), unless `remote_src` says the source is on the host; the first word of
/// `_raw_params` or of `cmd` for `script`; and the `filename` of each field of a `uri`
/// `body` in `form-multipart`. `assemble` takes its source from the host unless `remote_src` is
/// false.
fn check_sources(name: &str, ctx: &Context<'_>) -> Result<(), String> {
    let subdir = if matches!(name, "template" | "win_template") {
        "templates"
    } else {
        "files"
    };
    let check = |arg: &str, named: &str| {
        check_host_named(
            ctx.host_named_paths,
            ctx.args_untrusted,
            arg,
            named,
            subdir,
            ctx.origin,
            ctx.playbook_dir,
        )
    };
    let text = |arg: &str| {
        ctx.args
            .get(arg)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    // Only the plugins that read `remote_src` are told by it: `template`, `script` and a
    // collection's plugin read `src` off the controller whatever it says. `unarchive` reads
    // `copy: no` as `remote_src: yes` when `remote_src` is not given. `fetch` reads its `src`
    // off the host.
    let remote_src = ctx.args.get("remote_src");
    let remote = match name {
        "copy" | "uri" => remote_src.is_some_and(ansible_bool),
        "unarchive" => match (remote_src, ctx.args.get("copy")) {
            (Some(value), _) => ansible_bool(value),
            (None, copy) => copy.is_some_and(|copy| !ansible_bool(copy)),
        },
        "assemble" => remote_src.is_none_or(ansible_bool),
        "fetch" => true,
        _ => false,
    };
    if !remote {
        check("src", text("src"))?;
        if let Some(Value::Object(fields)) = ctx.args.get("body") {
            for field in fields.values().filter_map(Value::as_object) {
                let file = field.get("filename").and_then(Value::as_str);
                if let Some(file) = file.filter(|_| field.get("content").is_none_or(is_empty)) {
                    check("body", file)?;
                }
            }
        }
    }
    for arg in ["_raw_params", "cmd"] {
        // A script's file is the first word of its line, as Python's `shlex.split` reads it.
        // The `shlex` crate reads a word that starts with `#` as a comment, so a line that opens
        // with one is held to `contained` by the word as written.
        let line = text(arg).trim();
        if line.starts_with('#') {
            let word = line.split_whitespace().next().unwrap_or_default();
            if ctx.host_named_paths == HostNamedPaths::Contained && ctx.args_untrusted.contains(arg)
            {
                return Err(contained_refusal(arg, word));
            }
            check(arg, word)?;
            continue;
        }
        let first = shlex::split(line)
            .and_then(|words| words.into_iter().next())
            .unwrap_or_default();
        check(arg, &first)?;
    }
    Ok(())
}

/// Whether a `content` value is what Python reads as false.
fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Number(n) => n.as_f64() == Some(0.0),
    }
}

/// ansible-core's plugin `name`, which may run `modules` and no other module.
#[expect(clippy::too_many_lines)]
pub(super) fn start(
    name: &'static str,
    modules: &'static [&'static str],
    ctx: Context<'_>,
) -> Box<dyn Plugin> {
    // `find_needle` lets ansible-core's plugins read any controller file a `src` or a script's
    // line names, and put it on the host: a name a managed host chose is followed as far as
    // `host_named_paths` allows.
    if let Err(msg) = check_sources(name, &ctx) {
        return super::copy::failing(msg);
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
    if let Some(delegate) = &ctx.link.delegate {
        task.insert("delegate_to".into(), json!(delegate));
    }
    task.insert("check_mode".into(), Value::Bool(ctx.check_mode));
    task.insert("diff".into(), Value::Bool(ctx.diff));
    // What `ActionBase.run` reads to refuse an action that does not support it, and what a plugin
    // such as `uri` reads to ask for its module to be wrapped.
    if ctx.async_val > 0 {
        task.insert("async".into(), json!(ctx.async_val));
    }
    let task = Value::Object(task);
    // What the link is opened with comes from the host the module runs on, the delegate when
    // there is one. What the plugin reads as `task_vars` is the task's own host, and the delegate's
    // variables are in `ansible_delegated_vars`, as the reference hands them over.
    let running = flat_vars(ctx.running_vars);
    let mut task_vars = if ctx.delegated {
        let mut own = flat_vars(ctx.item_vars);
        own.insert(
            "ansible_delegated_vars".into(),
            Value::Object(running.clone()),
        );
        own
    } else {
        running.clone()
    };
    // Where the plugin looks for a `src`: the role's root, the directory of the task's file, then
    // the playbook's directory, as `Task.get_search_path` answers for a task that is in a role,
    // the roles it is a dependency of coming right after the role.
    let search: Vec<String> = ctx
        .origin
        .search_dirs()
        .chain([ctx.playbook_dir])
        .map(|p| p.display().to_string())
        .collect();
    task_vars.insert("_volant_search_path".into(), json!(search));
    // The connection options a plugin reads with `get_option` for the ssh arguments its own
    // `ssh` runs take (`synchronize` with `use_ssh_args`).
    task_vars.insert(
        "_volant_ssh_options".into(),
        json!({
            "ssh_common_args": ctx.link.ssh_common_args,
            "ssh_extra_args": ctx.link.ssh_extra_args,
        }),
    );
    let play_context = play_context_of(&ctx, &running);
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
        hostvars: Arc::clone(&ctx.running_vars.hostvars),
        untrusted_hosts: [
            Arc::clone(&ctx.item_vars.untrusted_hosts),
            Arc::clone(&ctx.running_vars.untrusted_hosts),
        ],
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
