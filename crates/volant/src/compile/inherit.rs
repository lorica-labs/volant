// SPDX-License-Identifier: GPL-3.0-or-later
//! Keyword inheritance down a block.

use serde_json::{Map, Value};

use crate::playbook::PlayTask;

pub(super) fn extend(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
}

pub(super) fn layered(under: &Map<String, Value>, over: &Map<String, Value>) -> Map<String, Value> {
    let mut out = under.clone();
    extend(&mut out, over);
    out
}

/// One task with the keywords of everything above it folded in. The inner value wins wherever it
/// says anything, and `when` concatenates outer first: measured on ansible-core 2.19.12, a block
/// with a false `when` and a task with a true one skips reporting the **block's** condition as
/// the `false_condition`, so the outer condition has to be evaluated first.
pub(super) fn merge(outer: &PlayTask, inner: &PlayTask) -> PlayTask {
    let mut task = inner.clone();
    task.when = outer
        .when
        .iter()
        .chain(&inner.when)
        .cloned()
        .collect::<Vec<_>>();
    // Measured: a block saying `ignore_errors: true` loses to a task saying `ignore_errors:
    // false` inside it, which is why the keyword is three-state all the way down here.
    task.ignore_errors = inner
        .ignore_errors
        .clone()
        .or_else(|| outer.ignore_errors.clone());
    task.timeout = inner.timeout.or(outer.timeout);
    task.throttle = inner.throttle.clone().or_else(|| outer.throttle.clone());
    task.debugger = inner.debugger.clone().or_else(|| outer.debugger.clone());
    // All three-state for the same reason `ignore_errors` is: a play or a block that says one
    // thing and a task that says the other have to be told apart from a task that says nothing.
    task.no_log = inner.no_log.or(outer.no_log);
    task.check_mode = inner
        .check_mode
        .clone()
        .or_else(|| outer.check_mode.clone());
    task.diff = inner.diff.clone().or_else(|| outer.diff.clone());
    // Three-state for the same reason, and inherited the same way: `Block.fattributes` carries
    // all three, `Play.fattributes` carries `run_once` alone, and the innermost value wins.
    task.run_once = inner.run_once.or(outer.run_once);
    task.any_errors_fatal = inner.any_errors_fatal.or(outer.any_errors_fatal);
    task.ignore_unreachable = inner.ignore_unreachable.or(outer.ignore_unreachable);
    task.delegate_facts = inner.delegate_facts.or(outer.delegate_facts);
    task.delegate_to = inner
        .delegate_to
        .clone()
        .or_else(|| outer.delegate_to.clone());
    // `environment` is the one keyword here that does not have a winner: measured on
    // ansible-core 2.19.12, a play's layer and a task's layer are both in force and the task's
    // wins name by name, so the layers are stacked outermost first and merged when the task runs.
    task.environment = outer
        .environment
        .iter()
        .chain(&inner.environment)
        .cloned()
        .collect();
    // `module_defaults` stacks layers like `environment`; the winner per module is settled when
    // the task runs, where the values are rendered.
    task.module_defaults = outer
        .module_defaults
        .iter()
        .chain(&inner.module_defaults)
        .cloned()
        .collect();
    task.collections = inner
        .collections
        .clone()
        .or_else(|| outer.collections.clone());
    task.connect = inner.connect.over(&outer.connect);
    task.r#become = inner.r#become.or(outer.r#become);
    task.become_user = inner
        .become_user
        .clone()
        .or_else(|| outer.become_user.clone());
    // Tags are a set, and they only ever grow downwards: measured on ansible-core 2.19.12, a
    // task inside a block tagged `outer` is selected by `--skip-tags outer` as surely as the
    // block is, and it lists as `TAGS: [inner, outer]` with its own. Sorted and deduplicated
    // here because every reader of the list treats it as a set - the listings sort what they
    // print, and the selection asks whether a name is in it.
    task.tags = inner
        .tags
        .iter()
        .chain(&outer.tags)
        .cloned()
        .collect::<Vec<_>>();
    task.tags.sort_unstable();
    task.tags.dedup();
    // A `notify` written on a block reaches the tasks under it - measured on ansible-core
    // 2.19.12, a block carrying `notify: h` runs `h` when a task inside it changes something -
    // and a task that notifies something of its own keeps both. Left in the order they were
    // written, with the task's own first: nothing depends on it, since handlers run in
    // definition order, but a refusal names the first name the playbook wrote.
    for name in &outer.notify {
        if !task.notify.contains(name) {
            task.notify.push(name.clone());
        }
    }
    // The outer map first, so a name the task sets itself keeps the task's value.
    let mut vars = outer.vars.clone();
    vars.extend(inner.vars.clone());
    task.vars = vars;
    task
}
