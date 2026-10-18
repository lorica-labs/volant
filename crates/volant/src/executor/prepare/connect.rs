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
pub(super) fn delegate_for(
    task: &PlayTask,
    vars: &HostVars,
    templar: &Templar,
) -> Result<Option<String>, TemplateError> {
    let Some(raw) = &task.delegate_to else {
        return Ok(None);
    };
    let name = if Templar::is_template(raw) {
        templar
            .render(raw, vars)
            .map_err(|err| {
                let hint = if task.loop_items.is_some() && raw.contains(&task.loop_var) {
                    ". A 'delegate_to' that changes per loop item is not supported yet: one batch runs over one link"
                        .to_string()
                } else {
                    String::new()
                };
                TemplateError(format!("rendering 'delegate_to' {raw}: {}{hint}", err.0))
            })?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                TemplateError(format!("'delegate_to' must render to a host name: {raw}"))
            })?
    } else {
        raw.clone()
    };
    Ok(Some(name).filter(|n| !n.is_empty()))
}

/// `role_path` and `ansible_search_path`, which a role's tasks and the `first_found` and
/// `template` lookups read to find files. Measured on ansible-core 2.19.12, in a role's task:
/// `role_path` is the role's directory and `ansible_search_path` is `[<role>, <role>/tasks,
/// <playbook>]`. Outside a role the path is `[<playbook>]` and `role_path` is not defined, so
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
    for dir in origin
        .role_dir
        .as_deref()
        .into_iter()
        .chain([origin.file_dir.as_path(), playbook_dir])
    {
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
