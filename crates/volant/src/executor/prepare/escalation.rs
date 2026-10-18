// SPDX-License-Identifier: GPL-3.0-or-later
//! Privilege escalation for one task on one host.

use serde_json::Value;

use crate::playbook::PlayTask;
use crate::template::{Templar, TemplateError};
use crate::transport::{BecomeMethod, ConnectionDefaults, Escalation};
use crate::vars::{HostVars, host_setting, omit_token};

use super::plan::PlayPlan;
use crate::executor::as_bool_value;

/// Drops every value the `omit` variable rendered to, at any depth. Measured against
/// ansible-core 2.19.12, where `omit` is a sentinel the templating engine removes from any
/// container it survives in, sequences included: a mapping loses the key and a list loses the
/// element. Only the value goes, never the container around it, so `{k: omit}` comes back as an
/// empty mapping and `[omit]` as an empty list.
pub(super) fn remove_omit(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| v.as_str() != Some(omit_token()));
            map.values_mut().for_each(remove_omit);
        }
        Value::Array(items) => {
            items.retain(|v| v.as_str() != Some(omit_token()));
            items.iter_mut().for_each(remove_omit);
        }
        _ => {}
    }
}

/// Resolves privilege escalation for one task: whether to escalate, to whom, through which
/// program, and with what password. `None` means the task runs as the connecting user.
/// `become_method`, `become_exe` and `become_flags` take the order the reference gives every
/// setting it maps to a variable: the host's variable, then the task's keyword over its block's
/// over its play's, then the defaults.
///
/// The order is measured, not assumed. Against `ansible-core 2.19.12`: a host's
/// `ansible_become` variable beats both keywords in both directions (a play `become: false`
/// with `ansible_become=true` ran as root, and a task `become: true` with
/// `ansible_become=false` ran as the invoking user), and between the keywords the task beats
/// the play. `ansible_become_user` follows the same order. The connection defaults, which carry
/// `ansible.cfg`, its environment variables and the command line, speak last.
#[expect(clippy::too_many_lines)]
pub(super) fn become_for(
    task: &PlayTask,
    play: &PlayPlan,
    vars: &HostVars,
    defaults: &ConnectionDefaults,
    templar: &Templar,
) -> Result<Option<Escalation>, TemplateError> {
    let on = host_setting(vars, "ansible_become")
        .and_then(as_bool_value)
        .or(task.r#become)
        .or(play.r#become)
        .unwrap_or(defaults.r#become);
    if !on {
        return Ok(None);
    }
    // The method is refused for the task that would actually use it, so a task escalating
    // nowhere is unaffected by a method it never runs. A variable beats the keyword, which beats
    // the defaults that carry `ansible.cfg` and the command line: this catches the value
    // reaching an escalating task through `group_vars`, `host_vars`, `--extra-vars` or a
    // `set_fact`, which the startup pass cannot see.
    let (method, source) = match host_setting(vars, "ansible_become_method").and_then(Value::as_str)
    {
        Some(method) => (method, "ansible_become_method"),
        None => (
            task.connect
                .become_method
                .as_deref()
                .unwrap_or(&defaults.become_method),
            "become_method",
        ),
    };
    let method = BecomeMethod::named(method)
        .ok_or_else(|| TemplateError(format!("{source} '{method}' is not supported yet")))?;
    // `become_exe` and `become_flags` in the same order, each a template or not, then split into
    // the words the escalation program is given.
    let program = |var: &str, keyword: &Option<String>, default: &Option<String>| {
        let raw = match host_setting(vars, var).and_then(Value::as_str) {
            Some(text) => text.to_string(),
            None => match keyword.as_ref().or(default.as_ref()) {
                Some(text) => text.clone(),
                None => return Ok(None),
            },
        };
        let key = var.trim_start_matches("ansible_");
        let text = if Templar::is_template(&raw) {
            match templar
                .render(&raw, vars)
                .map_err(|err| TemplateError(format!("rendering '{key}' {raw}: {}", err.0)))?
            {
                Value::String(text) => text,
                _ => return Err(TemplateError(format!("'{key}' must render to text: {raw}"))),
            }
        } else {
            raw
        };
        shlex::split(&text).map(Some).ok_or_else(|| {
            TemplateError(format!(
                "'{key}' has unbalanced quotes and cannot be split into words: {text}"
            ))
        })
    };
    let exe = program(
        "ansible_become_exe",
        &task.connect.become_exe,
        &defaults.become_exe,
    )?
    .unwrap_or_default();
    let flags = program(
        "ansible_become_flags",
        &task.connect.become_flags,
        &defaults.become_flags,
    )?;
    let user = match host_setting(vars, "ansible_become_user").and_then(Value::as_str) {
        Some(user) => user.to_string(),
        None => task
            .become_user
            .clone()
            .or_else(|| play.become_user.clone())
            .unwrap_or_else(|| defaults.become_user.clone()),
    };
    // `become_user: "{{ app_user }}"` is ordinary Ansible, and the rendered name keys the link,
    // so it is resolved before the connection is opened. It renders against the task's variables
    // and not one loop item's: the reference escalates per item, and this engine diverges there
    // because one batch is one message to one agent under one user. The hint below names the
    // keyword, and names the loop variable only on a task that actually loops.
    let user = if Templar::is_template(&user) {
        templar
            .render(&user, vars)
            .map_err(|err| {
                let hint = match (task.loop_items.is_some(), user.contains(&task.loop_var)) {
                    (true, true) => {
                        ". A 'become_user' that changes per loop item is not supported yet: one \
                         batch escalates to one user"
                            .to_string()
                    }
                    (false, true) => format!(
                        ". '{}' is only defined while a task loops, and this task has no 'loop'",
                        task.loop_var
                    ),
                    _ => String::new(),
                };
                TemplateError(format!("rendering 'become_user' {user}: {}{hint}", err.0))
            })?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                TemplateError(format!("'become_user' must render to a user name: {user}"))
            })?
    } else {
        user
    };
    let password = host_setting(vars, "ansible_become_password")
        .or_else(|| host_setting(vars, "ansible_become_pass"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| defaults.become_password.clone());
    Ok(Some(Escalation {
        method,
        user,
        password,
        exe,
        flags,
    }))
}

/// The task's own name for the `FAILED - RETRYING` line: templated against `vars`, and never
/// role-prefixed the way `task_name`'s banner is - measured, the retry line of a task inside a
/// role carries the task's own name rather than the `role : name` the banner shows.
pub(in crate::executor) fn retry_name(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> String {
    if Templar::is_template(&task.name) {
        templar
            .render(&task.name, vars)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| task.name.clone())
    } else {
        task.name.clone()
    }
}
