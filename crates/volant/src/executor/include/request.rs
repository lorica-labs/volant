// SPDX-License-Identifier: GPL-3.0-or-later
//! What one item of an include statement asks for.

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use crate::compile::{Compiled, IncludeKind, IncludeParams, IncludeRequest, IncludeTarget, Step};
use crate::playbook::PlayTask;
use crate::template::Templar;

use crate::executor::prepare::Item;

/// One item's request, or the result that item fails with.
pub(super) fn include_request(
    compiled: &Compiled,
    step: &Step,
    kind: IncludeKind,
    item: &Item,
    templar: &Templar,
) -> Result<IncludeRequest, TaskResult> {
    let task = &step.task;
    // A file that includes itself, or a ring of them, otherwise grows the step list for as long
    // as the run has memory. The reference has no ceiling here at all: measured, `a.yml`
    // including itself runs until Python's stack is gone and exits 250 with a traceback, four
    // thousand lines in. Refused instead, at the depth the compiler's own recursions share.
    if step.origin.depth as usize + 1 > crate::compile::DEPTH {
        return Err(TaskResult::failed_with(format!(
            "includes nest deeper than {} levels: a file or a role that includes itself",
            crate::compile::DEPTH
        )));
    }
    // Only the statement's own `vars:` and the loop variable travel down, seeded with what the
    // statement above handed down so a chain of includes carries the whole chain. Measured on
    // ansible-core 2.19.12: an included task reads the statement's `vars:`, and everything else
    // the statement writes itself stops there - its `tags` do not descend, its `when` is
    // evaluated for the statement alone, its `no_log` censors its own line and not the tasks
    // behind it, `become` and `environment` are refused at load time. What the layers around
    // the statement gave it does descend, and travels in its origin.
    let mut vars = step.include_params.as_deref().cloned().unwrap_or_default();
    // Rendered from what the statement wrote, not read back out of the host's merged scope: the
    // scope resolves a name the host also carries a fact for to the **fact**, and the whole point
    // of the measurement above is that an include's own value wins there.
    //
    // A render that read a name from a managed host hands back that host's text: the name goes
    // down with the value, so the steps below treat it as the data it is.
    let mut own = Map::new();
    for (key, raw) in &task.vars {
        match templar.render_value_tainted(raw, &item.vars) {
            Ok((value, tainted)) => {
                own.insert(key.clone(), value.clone());
                if tainted {
                    vars.untrusted.insert(key.clone());
                } else {
                    vars.untrusted.remove(key);
                }
                vars.values.insert(key.clone(), value);
            }
            Err(err) => {
                return Err(TaskResult::failed_with(format!("Task failed: {}", err.0)));
            }
        }
    }
    if item.element.is_some() {
        bind_loop(task, item, &mut vars, &mut own);
    }
    let mut role_id = None;
    let (target, what) = match kind {
        IncludeKind::Tasks => {
            let name = item
                .args
                .get("file")
                .or_else(|| item.args.get("_raw_params"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    TaskResult::failed_with("Task failed: 'include_tasks' takes a file name")
                })?;
            let path = crate::compile::beside_or_in_role(
                &step.origin.file_dir,
                step.origin.role_dir.as_deref(),
                name,
            );
            if !path.is_file() {
                // The reference's own three sentences, measured word for word, with the file
                // named as the playbook wrote it and the path it looked in spelled out. The
                // `errno` tail is Python's; this engine reaches the same conclusion by asking the
                // filesystem, so the sentence is kept and the number with it.
                let mut body = Map::new();
                body.insert("changed".into(), json!(false));
                body.insert("include".into(), json!(name));
                body.insert(
                    "reason".into(),
                    json!(format!(
                        "Could not find or access '{p}' on the Ansible Controller: Unable to retrieve file contents.\nCould not find or access '{p}' on the Ansible Controller.\nIf you are using a module and expect the file to exist on the remote, see the remote_src option: [Errno 2] No such file or directory: '{p}'",
                        p = path.display()
                    )),
                );
                return Err(TaskResult(body));
            }
            // Shown the way the reference shows it: absolute, with `.` and `..` folded out.
            let what = crate::roles::folded(&path).display().to_string();
            (IncludeTarget::File(path), what)
        }
        IncludeKind::Role => {
            let entry = crate::compile::include_role_entry(&item.args)
                .map_err(|err| refused_statement(task, &err))?;
            // Measured on ansible-core 2.19.12: a role an `include_role` cannot find is an
            // ordinary task failure, `fatal: [h1]: FAILED! => {"changed": false, "reason": "the
            // role 'nosuchrole' was not found in <paths>"}`, exit 2 and a recap - and not the
            // exit 1 before the first banner a `roles:` entry gets, because by then the play is
            // already running.
            compiled.search.locate(&entry.name).map_err(|err| {
                let mut body = Map::new();
                body.insert("changed".into(), json!(false));
                body.insert("reason".into(), json!(format!("{err:#}")));
                TaskResult(body)
            })?;
            let what = entry.name.clone();
            role_id = Some(crate::compile::role_identity(&entry, &own));
            (IncludeTarget::Role(Box::new(entry)), what)
        }
    };
    Ok(IncludeRequest {
        target,
        what,
        vars,
        label: item.label.clone(),
        role_id,
    })
}

/// What the loop binds for one item, handed down with the statement's variables and kept apart
/// as part of a role's identity: measured, the same role named by the same item twice is one
/// instance and by two items is two.
fn bind_loop(task: &PlayTask, item: &Item, vars: &mut IncludeParams, own: &mut Map<String, Value>) {
    let bound = [Some(&task.loop_var), task.index_var.as_ref()]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .chain(["ansible_loop"]);
    for name in bound {
        if let Some(value) = item.vars.get_whole(name) {
            if item.vars.untrusted.contains(name) {
                vars.untrusted.insert(name.to_string());
            }
            vars.values.insert(name.to_string(), value.clone());
            own.insert(name.to_string(), value);
        }
    }
    let var = Value::String(task.loop_var.clone());
    vars.values.insert("ansible_loop_var".into(), var.clone());
    own.insert("ansible_loop_var".into(), var);
    if let Some(index) = &task.index_var {
        own.insert("ansible_index_var".into(), Value::String(index.clone()));
    }
}

/// The failure of a statement the reference reads and refuses, as a task failure with the
/// `changed: false` it gives every failure of an include.
///
/// A boolean option it cannot convert is chained as the reference chains it: the keyword and the
/// sentence, both at the value.
fn refused_statement(task: &PlayTask, err: &anyhow::Error) -> TaskResult {
    let text = format!("{err:#}");
    let mut body = Map::new();
    body.insert("changed".into(), json!(false));
    body.insert("msg".into(), json!(format!("Task failed: {text}")));
    if let Some((key, sentence)) = text
        .strip_prefix("Error processing keyword '")
        .and_then(|rest| rest.split_once("': "))
    {
        let at = task.value_origins.get(&format!("args.{key}"));
        body.insert(
            crate::render::CHAIN_KEY.into(),
            json!([
                crate::render::cause(&format!("Error processing keyword '{key}'."), at, None),
                crate::render::cause(sentence, at, None)
            ]),
        );
    }
    TaskResult(body)
}
