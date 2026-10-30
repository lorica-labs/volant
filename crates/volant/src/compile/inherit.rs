// SPDX-License-Identifier: GPL-3.0-or-later
//! Keyword inheritance down a block.

use serde_json::{Map, Value};

use crate::playbook::PlayTask;
use crate::roles::{OwnVars, RoleVars};

pub(super) fn extend(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
}

/// Lays what the rest of the play lends under one role's own values.
///
/// Measured on ansible-core 2.19.12: a role sees the `defaults` and `vars` every public role of the
/// play holds, its own over them. `group` is what the roles of the same statement lend each
/// other, a dependency and the role that asked for it, public or not; it is kept with the role's
/// own values so that the next public statement can lay its variables under the role again
/// without losing it.
pub(crate) fn lay_under(
    role: &mut RoleVars,
    exported: &RoleVars,
    group: Option<(Map<String, Value>, Map<String, Value>)>,
) {
    let own = role.own.get_or_insert_with(|| {
        Box::new(OwnVars {
            defaults: role.defaults.clone(),
            vars: role.vars.clone(),
            ..OwnVars::default()
        })
    });
    if let Some((defaults, vars)) = group {
        own.group_defaults = defaults;
        own.group_vars = vars;
    }
    role.defaults = stacked(&[
        &exported.defaults,
        &own.group_defaults,
        &role.inherited_defaults,
        &own.defaults,
    ]);
    role.vars = stacked(&[
        &exported.vars,
        &own.group_vars,
        &role.inherited_vars,
        &own.vars,
    ]);
}

/// Layers in one copy, the lowest first.
fn stacked(layers: &[&Map<String, Value>]) -> Map<String, Value> {
    let mut out = Map::new();
    for layer in layers {
        extend(&mut out, layer);
    }
    out
}

/// A keyword that may be a template, as the layer that wrote it says: the inner layer when it
/// wrote the keyword in either form, the outer one otherwise.
fn layered_keyword<T: Copy>(
    inner: (Option<T>, &Option<String>),
    outer: (Option<T>, &Option<String>),
) -> (Option<T>, Option<String>) {
    let (value, template) = if inner.0.is_some() || inner.1.is_some() {
        inner
    } else {
        outer
    };
    (value, template.clone())
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
    // The keywords that may be templates: the inner layer wins whichever form it is written in, a
    // literal or a template, and takes both fields with it, so that an inner `false` hides an
    // outer template and an inner template hides an outer `true`.
    let (timeout, timeout_template) = layered_keyword(
        (inner.timeout, &inner.deferred.timeout),
        (outer.timeout, &outer.deferred.timeout),
    );
    task.timeout = timeout;
    task.deferred.timeout = timeout_template;
    task.throttle = inner.throttle.clone().or_else(|| outer.throttle.clone());
    task.debugger = inner.debugger.clone().or_else(|| outer.debugger.clone());
    // All three-state for the same reason `ignore_errors` is: a play or a block that says one
    // thing and a task that says the other have to be told apart from a task that says nothing.
    (task.no_log, task.deferred.no_log) = layered_keyword(
        (inner.no_log, &inner.deferred.no_log),
        (outer.no_log, &outer.deferred.no_log),
    );
    task.check_mode = inner
        .check_mode
        .clone()
        .or_else(|| outer.check_mode.clone());
    task.diff = inner.diff.clone().or_else(|| outer.diff.clone());
    // Three-state for the same reason, and inherited the same way: `Block.fattributes` carries
    // all three, `Play.fattributes` carries `run_once` alone, and the innermost value wins.
    (task.run_once, task.deferred.run_once) = layered_keyword(
        (inner.run_once, &inner.deferred.run_once),
        (outer.run_once, &outer.deferred.run_once),
    );
    (task.any_errors_fatal, task.deferred.any_errors_fatal) = layered_keyword(
        (inner.any_errors_fatal, &inner.deferred.any_errors_fatal),
        (outer.any_errors_fatal, &outer.deferred.any_errors_fatal),
    );
    (task.ignore_unreachable, task.deferred.ignore_unreachable) = layered_keyword(
        (inner.ignore_unreachable, &inner.deferred.ignore_unreachable),
        (outer.ignore_unreachable, &outer.deferred.ignore_unreachable),
    );
    (task.delegate_facts, task.deferred.delegate_facts) = layered_keyword(
        (inner.delegate_facts, &inner.deferred.delegate_facts),
        (outer.delegate_facts, &outer.deferred.delegate_facts),
    );
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
    (task.r#become, task.deferred.r#become) = layered_keyword(
        (inner.r#become, &inner.deferred.r#become),
        (outer.r#become, &outer.deferred.r#become),
    );
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
