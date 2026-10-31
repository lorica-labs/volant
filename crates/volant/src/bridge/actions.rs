// SPDX-License-Identifier: GPL-3.0-or-later
//! Action plugins, each run in a helper of the pool while it runs.

use std::sync::MutexGuard;
use std::time::Instant;

use serde_json::{Map, Value, json};
use volant_protocol::frame::MAX_FRAME_LEN;

use super::conversation::converse;
use super::{Actions, Ask, Bridge, BridgeError, Reply};
use crate::python::ActionHelper;

/// One action plugin to run: `task` holds the task's keywords and `args`, `task_vars` the host's
/// variables, `play_context` the fields of ansible-core's `PlayContext`. `link` is
/// `{"kind": "owned", "connection": <plugin>, "become": <plugin>, "verbosity": <n>}` for a host the
/// helper reaches through a connection plugin of its own, whose value then carries what `Display`
/// said as `_volant_display`; `None` gives the plugin the stand-in connection that asks the
/// controller.
pub struct ActionCall<'a> {
    pub name: &'a str,
    pub task: &'a Value,
    pub task_vars: &'a Value,
    pub play_context: &'a Value,
    pub link: Option<&'a Value>,
}

impl Bridge {
    /// Runs the action plugin `call` names for one task in a helper of the pool, as ansible-core
    /// runs it. Each [`Ask`] the plugin makes is answered by `asks`, one at a time. The value is
    /// the plugin's result, a failed one when the plugin raised; it is data from the host and is
    /// never rendered again.
    ///
    /// # Errors
    /// [`BridgeError::NotFound`] when ansible-core has no such action plugin, `Plugin` when the
    /// request is refused, `Start` when no helper starts, and `Helper` when the helper dies,
    /// breaks the protocol or is still running at `deadline`: every
    /// wait for the helper ends there. A helper that ended any of those
    /// ways is killed, and the next action starts another.
    pub fn run_action(
        &self,
        call: &ActionCall<'_>,
        deadline: Instant,
        asks: &mut dyn FnMut(Ask) -> Reply,
    ) -> Result<Value, BridgeError> {
        let name = call.name;
        let (mut helper, id) = self.action_helper()?;
        // Built as maps, not with `json!`, which serializes the values again and refuses a `NaN`
        // a variable holds; the helper's `json.loads` reads it.
        let action = Map::from_iter([
            ("name".to_string(), json!(name)),
            ("task".to_string(), call.task.clone()),
            ("task_vars".to_string(), call.task_vars.clone()),
            ("play_context".to_string(), call.play_context.clone()),
            (
                "link".to_string(),
                call.link.cloned().unwrap_or(Value::Null),
            ),
        ]);
        let request = Value::Object(Map::from_iter([
            ("id".to_string(), json!(id)),
            ("action".to_string(), Value::Object(action)),
        ]))
        .to_string();
        if request.len() > MAX_FRAME_LEN {
            self.lock_actions().idle.push(helper);
            return Err(BridgeError::Helper(format!(
                "the request for the action plugin '{name}' is {} bytes, over the {MAX_FRAME_LEN} byte limit",
                request.len()
            )));
        }
        match converse(&mut helper, id, name, &request, deadline, asks) {
            Err(BridgeError::Helper(why)) => {
                // What the helper left unsaid is not read: it is ended, and the pool loses it.
                let status = helper.end();
                Err(BridgeError::Helper(
                    if why.starts_with("the Python helper exited") {
                        format!("{why} ({status})")
                    } else {
                        why
                    },
                ))
            }
            mut outcome => {
                self.lock_actions().idle.push(helper);
                // The persistent process a task started is the host's until a reset ends it.
                if let Ok(Value::Object(map)) = &mut outcome
                    && let Some(Value::String(path)) = map.remove("_volant_socket")
                    && let Some(host) = call
                        .task_vars
                        .get("inventory_hostname")
                        .and_then(Value::as_str)
                {
                    self.note_socket(host, &path);
                }
                outcome
            }
        }
    }

    fn lock_actions(&self) -> MutexGuard<'_, Actions> {
        self.actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// An idle helper of the pool, or a new one, and the id of the request it is about to get.
    fn action_helper(&self) -> Result<(ActionHelper, u64), BridgeError> {
        let mut actions = self.lock_actions();
        if let Some(why) = &actions.broken {
            return Err(why.clone());
        }
        actions.next += 1;
        let id = actions.next;
        // A helper that died while idle is dropped here, which reaps it, and never reused.
        while let Some(mut helper) = actions.idle.pop() {
            if helper.alive() {
                return Ok((helper, id));
            }
        }
        // Started without the lock: an action on another host need not wait for this start.
        drop(actions);
        match (self.start)() {
            Ok(helper) => Ok((ActionHelper::new(helper), id)),
            Err(err) => {
                let why = BridgeError::Start(format!("{err:#}"));
                self.lock_actions().broken = Some(why.clone());
                Err(why)
            }
        }
    }
}
