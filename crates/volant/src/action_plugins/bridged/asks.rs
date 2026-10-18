// SPDX-License-Identifier: GPL-3.0-or-later
//! What each ask of the helper becomes on the host, and what the host answers.

use std::time::{Duration, Instant};

use serde_json::Value;
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use super::super::files::fits;
use super::super::{Step, Sub, is_gone};
use super::helpers::{command_reply, failure_or, plain_path, raw};
use super::{Bridged, Pending};
use crate::bridge::{Ask, BridgeError, Reply, TmpOp};

impl Bridged {
    /// The step an ask becomes, or the reply it gets without one.
    #[expect(clippy::too_many_lines)]
    pub(super) fn step_for(&mut self, ask: Ask) -> Result<(Step, Pending), Reply> {
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
    pub(super) fn reply_for(&mut self, pending: Pending, last: Option<TaskResult>) -> Reply {
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
    pub(super) fn result_of(&self, outcome: Result<Value, BridgeError>) -> TaskResult {
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
    pub(super) fn sweep(path: &str) -> Sub {
        Sub {
            module: "raw",
            args: raw(format!("rm -f -r '{path}'")),
            files: Vec::new(),
        }
    }
}
