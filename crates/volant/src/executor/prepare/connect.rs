// SPDX-License-Identifier: GPL-3.0-or-later
//! Connection and delegation settings for one task.

use std::path::Path;

use serde_json::Value;

use crate::compile::Origin;
use crate::playbook::PlayTask;
use crate::template::{Templar, TemplateError};
use crate::transport::ConnectionKeywords;
use crate::vars::HostVars;

/// The task's `remote_user`, `port` and `connection`, rendered once for the task against its
/// variables, for the reason `delegate_for` renders `delegate_to` that way. An empty result is
/// no keyword, which is what `"{{ maybe | default('') }}"` renders to when nobody asked.
pub(super) fn connect_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<ConnectionKeywords, TemplateError> {
    let render = |key: &str, raw: &Option<String>| -> Result<Option<String>, TemplateError> {
        let Some(raw) = raw else {
            return Ok(None);
        };
        let text = if Templar::is_template(raw) {
            match templar
                .render(raw, vars)
                .map_err(|err| TemplateError(format!("rendering '{key}' {raw}: {}", err.0)))?
            {
                Value::String(text) => text,
                // `port: "{{ ssh_port }}"` with a number behind it.
                Value::Number(number) => number.to_string(),
                _ => return Err(TemplateError(format!("'{key}' must render to text: {raw}"))),
            }
        } else {
            raw.clone()
        };
        Ok(Some(text.trim().to_string()).filter(|text| !text.is_empty()))
    };
    let connect = &task.connect;
    let port = render("port", &connect.port)?
        .map(|text| {
            text.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| {
                    TemplateError(format!("'port' must be a port number, found '{text}'"))
                })
        })
        .transpose()?;
    // A value a template read from a host can end up here, so what reaches `ssh` is held to the
    // shape of a name: nothing that starts like an option, splits into words or ends a line.
    let user = render("remote_user", &connect.remote_user)?;
    if let Some(user) = &user
        && (user.starts_with('-')
            || user
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '"'))
    {
        return Err(TemplateError(format!(
            "'remote_user' must be a user name, found '{}'",
            user.escape_debug()
        )));
    }
    let connection = render("connection", &connect.connection)?;
    if let Some(name) = &connection
        && !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
    {
        return Err(TemplateError(format!(
            "'connection' must be a plugin name, found '{}'",
            name.escape_debug()
        )));
    }
    Ok(ConnectionKeywords {
        user,
        port,
        connection,
    })
}

/// `delegate_to`, rendered once for the task. An empty name is no delegation, which is what
/// `delegate_to: "{{ maybe | default('') }}"` renders to when nobody asked for one.
///
/// Rendered against the task's variables and **not** against one loop item's, the way
/// `become_user` is and for the same reason: one batch is one message to one agent over one
/// link, and a task whose items each name a different delegate would have to split across links
/// and interleave the answers. The reference does delegate per item; that divergence is named
/// here rather than left to look like the operator's own typo.
///
/// The flag says the name is data: the render read a value that came from a managed host or a
/// file read at run time. Such a name is a host name whatever it holds, and the view built for
/// it renders none of the names it derives from it.
/// Fails each item of a controller-side task on a host whose connection plugin does not exist,
/// with the loader's warning in `warnings`. Measured on ansible-core 2.19.12: every task of the
/// host loads its connection plugin, so `set_fact` and `debug` fail with `Task failed: the
/// connection plugin 'x' was not found` as a module would; a delegated one reads the delegate's.
pub(super) fn unplugged(
    task: &PlayTask,
    host: &str,
    (vars, templar, defaults): (&HostVars, &Templar, &crate::transport::ConnectionDefaults),
    items: &mut [super::Item],
    warnings: &mut Vec<String>,
) {
    let Some((failure, warning)) = missing_plugin(task, host, vars, templar, defaults) else {
        return;
    };
    warnings.extend(warning);
    for item in items.iter_mut().filter(|i| i.skipped.is_none()) {
        item.skipped = Some(failure.clone());
    }
}

fn missing_plugin(
    task: &PlayTask,
    host: &str,
    vars: &HostVars,
    templar: &Templar,
    defaults: &crate::transport::ConnectionDefaults,
) -> Option<(volant_protocol::TaskResult, Option<String>)> {
    let keywords = connect_for(task, vars, templar).ok()?;
    let err =
        crate::transport::Transport::for_task(host, &vars.own_variables(), defaults, &keywords)
            .err()?;
    let missing = err.downcast_ref::<crate::transport::ConnectionFailsTask>()?;
    let mut result = volant_protocol::TaskResult::failed_with(format!("Task failed: {missing}"));
    if volant_protocol::modules::short_name(&task.module) != "debug" {
        result.0.insert("changed".into(), Value::Bool(false));
    }
    Some((result, missing.warning()))
}

pub(super) fn delegate_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<Option<(String, bool)>, TemplateError> {
    let Some(raw) = &task.delegate_to else {
        return Ok(None);
    };
    let (name, data) = if Templar::is_template(raw) {
        let (rendered, data) = templar
            .render_value_tainted(&Value::String(raw.clone()), vars)
            .map_err(|err| {
                let hint = if task.loop_items.is_some() && raw.contains(&task.loop_var) {
                    ". A 'delegate_to' that changes per loop item is not supported yet: one batch runs over one link"
                        .to_string()
                } else {
                    String::new()
                };
                TemplateError(format!("rendering 'delegate_to' {raw}: {}{hint}", err.0))
            })?;
        let name = rendered.as_str().map(str::to_string).ok_or_else(|| {
            TemplateError(format!("'delegate_to' must render to a host name: {raw}"))
        })?;
        (name, data)
    } else {
        (raw.clone(), false)
    };
    Ok(Some((name, data)).filter(|(n, _)| !n.is_empty()))
}

/// `role_path` and `ansible_search_path`, which a role's tasks and the `first_found` and
/// `template` lookups read to find files. Measured on ansible-core 2.19.12, in a role's task:
/// `role_path` is the role's directory and `ansible_search_path` is `[<role>, <role>/tasks,
/// <playbook>]`, with the roles it is a dependency of right after the role. Outside a role the path is `[<playbook>]` and `role_path` is not defined, so
/// reading it fails on the undefined name, as in the reference.
///
/// The middle entry is the directory of the file the task was written in, and a directory already
/// listed is not listed again, which is how a playbook's own task ends up with one entry.
/// Inserted as author content, and into the raw map a task's own `vars:` resolve against rather
/// than into the resolved `HostVars` afterwards: a `vars:` built from a lookup that needs one of
/// these two names (`first_found`, `template`) has to find it on the same pass that resolves the
/// task's variables, or it is left as the unrendered call, with no later pass to pick it back up.
/// A host fact of the same name does not outlive them, so `untrusted` drops both names too.
pub(super) fn insert_search_path(vars: &mut HostVars, origin: &Origin, playbook_dir: &Path) {
    let mut search: Vec<&Path> = Vec::new();
    for dir in origin.search_dirs().chain([playbook_dir]) {
        if !search.contains(&dir) {
            search.push(dir);
        }
    }
    let search = search
        .iter()
        .map(|dir| Value::String(dir.display().to_string()))
        .collect();
    // `HostVars::insert`: the engine's own value beats a gathered fact of the same name, and it
    // is trusted whatever the name held before.
    vars.insert("ansible_search_path".into(), Value::Array(search));
    if let Some(role) = &origin.role_dir {
        vars.insert(
            "role_path".into(),
            Value::String(role.display().to_string()),
        );
    }
}
