// SPDX-License-Identifier: GPL-3.0-or-later
//! The line an include item shows, and what it counts as.

use serde_json::{Value, json};
use volant_protocol::TaskResult;

use crate::playbook::PlayTask;
use crate::stats::Outcome;

/// One item of an include statement that has a line to show: a `when` left it out, or resolving
/// what it named went wrong.
pub(in crate::executor) struct Shown {
    pub(super) element: Option<Value>,
    pub(super) label: Option<String>,
    pub(super) result: TaskResult,
    /// Whether this is a failure rather than a skip. Carried here rather than read off the
    /// result's own `failed` key, because the reference's `fatal:` line for an include is
    /// `{"changed": false, "include": "nosuch.yml", "reason": "..."}` - measured, with no
    /// `failed` in it - and a key put there to be classified by would be a key on the line.
    pub(super) failed: bool,
    /// A file that could not be loaded: the reference prints its reason in an `[ERROR]` block,
    /// and in a loop prints no line of its own for the item.
    pub(super) load_failed: bool,
    /// The statement's `ignore_errors` as `prepare` rendered it for this item, when it is a
    /// template; `None` reads the keyword as written.
    pub(super) ignored: Option<bool>,
}

impl Shown {
    /// What this item counts as, and the result its line carries. A file that could not be loaded
    /// fails the host from the strategy, not the task: measured on ansible-core 2.19.12, a rescue
    /// still runs and the run exits 0, but the recap counts a failure for each such item and no
    /// rescue.
    pub(super) fn outcome(&self, ignored: bool, rescuable: bool) -> (Outcome, TaskResult) {
        let outcome = match (self.failed, ignored) {
            (false, _) => Outcome::Skipped,
            (true, true) => Outcome::Ignored,
            (true, false) if rescuable && !self.load_failed => Outcome::Rescued,
            (true, false) => Outcome::Failed,
        };
        let mut result = self.result.clone();
        if self.load_failed && rescuable && outcome == Outcome::Failed {
            result.0.insert(crate::stats::RESCUED.into(), json!(true));
        }
        (outcome, result)
    }
}

/// The `[ERROR]` block of one item's line, when it has one.
pub(super) fn item_error(
    item: &Shown,
    task: &PlayTask,
    censored: bool,
) -> Option<crate::render::ErrorBlock> {
    if item.load_failed {
        return Some(load_error(&item.result));
    }
    (item.failed && item.result.0.contains_key(crate::render::CHAIN_KEY))
        .then(|| crate::render::ErrorBlock::of(&item.result, task.origin.as_ref(), censored))
}

/// The key a failure carries when the rescue that takes it reads no `ansible_failed_result` and
/// no `ansible_failed_task`.
pub(in crate::executor) const NO_FAILED_VARS: &str = "_volant_no_failed_vars";

/// The `[ERROR]` block of a file an include could not load: its reason, and no place.
fn load_error(result: &TaskResult) -> crate::render::ErrorBlock {
    crate::render::ErrorBlock {
        message: result
            .0
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        origin: None,
        unknown: None,
        causes: Vec::new(),
        lead: None,
        on_stderr: true,
    }
}
