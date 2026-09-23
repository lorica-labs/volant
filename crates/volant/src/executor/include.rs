// SPDX-License-Identifier: GPL-3.0-or-later
//! Resolving a dynamic `include_tasks`, `import_tasks` or `include_role` statement for one host.

use std::sync::Mutex;

use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use volant_protocol::TaskResult;

use crate::compile::{Compiled, IncludeKind, IncludeParams, IncludeRequest, IncludeTarget, Step};
use crate::playbook::PlayTask;
use crate::render::Dump;
use crate::stats::Outcome;
use crate::template::Templar;
use crate::transport::ConnectionDefaults;
use crate::vars::VarStore;

use super::coordinator::{Event, Progress};
use super::prepare::{Item, PlayPlan, Prepared, prepare};
use super::run::{classify, empty_loop_result, registered_value};

/// One expansion a host asked for at an include step.
pub(super) struct IncludeGroup {
    /// What two hosts have to agree on to share one `included:` line. See
    /// [`crate::compile::IncludeRequest::key`].
    pub(super) key: String,
    /// What that line names: the file's absolute path, or the role's bare name.
    pub(super) what: String,
    /// The loop item's label, for the `=> (item=...)` the line carries.
    pub(super) label: Option<String>,
    /// What the statement hands down to everything it brought in.
    pub(super) params: IncludeParams,
    /// The steps, blocks, roles and handlers the statement brought in, numbered from zero.
    pub(super) expanded: Compiled,
}

/// What one host asks for at an include step, and the lines it has to show for the items that
/// asked for nothing.
///
/// The whole of the resolution happens here, on the driver, and every way it can go wrong comes
/// back as a `TaskResult` rather than as an error: a file that is not there, a file that is not a
/// list of tasks, a role nobody can find and a statement nested past the ceiling all fail **this
/// host's** task, which is what lets a `rescue` around the statement take them - measured on
/// ansible-core 2.19.12, an `include_tasks` in a block's body whose file is missing is rescued
/// like any other failure - and what lets the other hosts carry on.
///
/// `warnings` collects what preparing the statement's own arguments raised, for the caller to
/// put on the coordinator's queue.
#[expect(
    clippy::too_many_arguments,
    reason = "the driver's whole context, needed to resolve one include"
)]
pub(super) fn resolve_include(
    compiled: &Compiled,
    step: &Step,
    kind: IncludeKind,
    host: &str,
    plan: &PlayPlan,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
    defaults: &ConnectionDefaults,
    warnings: &mut Vec<String>,
) -> (Vec<IncludeGroup>, Vec<Shown>) {
    let items = match prepare(step, host, plan, live, templar, store, defaults, warnings) {
        Ok(
            Prepared::Skipped(items)
            | Prepared::Local(items, _)
            | Prepared::Remote(items, _, _, _, _),
        ) => items,
        // A `when` that cannot be evaluated or a `loop` that is not a list, reported with the
        // reference's own prefix for a task that dies before it runs. Not deferred the way an
        // ordinary task's is: this arm is reached with the batch empty, so there is nothing to
        // send out first.
        Err(err) => {
            return (
                Vec::new(),
                vec![Shown {
                    element: None,
                    label: None,
                    result: TaskResult::failed_with(format!("Task failed: {}", err.0)),
                    failed: true,
                    ignored: None,
                }],
            );
        }
    };
    let mut groups = Vec::new();
    let mut shown = Vec::new();
    for item in items {
        if let Some(skipped) = &item.skipped {
            shown.push(Shown {
                element: item.element.clone(),
                label: item.label.clone(),
                result: skipped.clone(),
                failed: false,
                ignored: None,
            });
            continue;
        }
        let result = match include_request(compiled, step, kind, &item, templar) {
            // The file is there and cannot be used: it holds a mapping rather than a list of
            // tasks, its YAML does not parse, or something inside it names a module, an option
            // or a keyword this release refuses. Measured on ansible-core 2.19.12 for the first
            // of those:
            // `fatal: [h1]: FAILED! => {"changed": false, "include": "mapping.yml", "reason":
            // "included task files must contain a list of tasks"}`, exit 2, with a recap.
            Ok(request) => match crate::compile::expand_include(compiled, step, &request) {
                Ok(expanded) => {
                    groups.push(IncludeGroup {
                        key: request.key(),
                        what: request.what,
                        label: request.label,
                        params: request.vars,
                        expanded,
                    });
                    continue;
                }
                Err(err) => {
                    let mut body = Map::new();
                    body.insert("changed".into(), json!(false));
                    body.insert("include".into(), json!(request.what));
                    body.insert("reason".into(), json!(format!("{err:#}")));
                    TaskResult(body)
                }
            },
            Err(result) => result,
        };
        shown.push(Shown {
            element: item.element,
            label: item.label,
            result,
            failed: true,
            ignored: item.ignore_errors,
        });
    }
    (groups, shown)
}

/// One item of an include statement that has a line to show: a `when` left it out, or resolving
/// what it named went wrong.
pub(super) struct Shown {
    element: Option<Value>,
    label: Option<String>,
    result: TaskResult,
    /// Whether this is a failure rather than a skip. Carried here rather than read off the
    /// result's own `failed` key, because the reference's `fatal:` line for an include is
    /// `{"changed": false, "include": "nosuch.yml", "reason": "..."}` - measured, with no
    /// `failed` in it - and a key put there to be classified by would be a key on the line.
    failed: bool,
    /// The statement's `ignore_errors` as `prepare` rendered it for this item, when it is a
    /// template; `None` reads the keyword as written.
    ignored: Option<bool>,
}

/// One item's request, or the result that item fails with.
fn include_request(
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
    for (key, raw) in &task.vars {
        match templar.render_value_tainted(raw, &item.vars) {
            Ok((value, tainted)) => {
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
        if let Some(value) = item.vars.get(&task.loop_var) {
            if item.vars.untrusted.contains(&task.loop_var) {
                vars.untrusted.insert(task.loop_var.clone());
            }
            vars.values.insert(task.loop_var.clone(), value.clone());
        }
        vars.values.insert(
            "ansible_loop_var".into(),
            Value::String(task.loop_var.clone()),
        );
    }
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
            let what = path.display().to_string();
            (IncludeTarget::File(path), what)
        }
        IncludeKind::Role => {
            let entry = crate::compile::include_role_entry(&item.args)
                .map_err(|err| TaskResult::failed_with(format!("Task failed: {err:#}")))?;
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
            (IncludeTarget::Role(Box::new(entry)), what)
        }
    };
    Ok(IncludeRequest {
        target,
        what,
        vars,
        label: item.label.clone(),
    })
}

/// Sends the lines an include step shows and its `TaskDone`, and answers with the result a rescue
/// would be given when it failed for good.
///
/// It is not [`super::report::report_task`] because of what an include **does not** count.
/// Measured on ansible-core 2.19.12: a statement that brought something in counts one `ok` per
/// host per item, and that one is counted by the coordinator when it prints the `included:`
/// line - so the aggregate a looping statement would otherwise contribute has to count nothing,
/// or a two-item loop would read three. The two aggregates that do count are the ones with no
/// `included:` line behind them: a loop over an empty list, which shows `skipping:` and counts
/// it, and a loop an item failed in, which counts the failure without a line of its own.
pub(super) async fn report_include(
    tx: &mpsc::Sender<Event>,
    host: &str,
    index: usize,
    task: &PlayTask,
    shown: &[Shown],
    rescuable: bool,
    nothing_asked: bool,
) -> Option<TaskResult> {
    let is_loop = task.loop_items.is_some();
    let censored = task.censors();
    let mut any_failed = false;
    // A failure no item's `ignore_errors` swallowed: that is what fails the host.
    let mut unignored = false;
    let mut failure: Option<TaskResult> = None;
    for item in shown {
        let ignored = item.ignored.unwrap_or_else(|| task.ignores_errors());
        unignored |= item.failed && !ignored;
        let outcome = match (item.failed, ignored) {
            (false, _) => Outcome::Skipped,
            (true, true) => Outcome::Ignored,
            (true, false) if rescuable => Outcome::Rescued,
            (true, false) => Outcome::Failed,
        };
        if item.failed {
            if failure.is_none() {
                failure = Some(item.result.clone());
            }
            any_failed = true;
        }
        let _ = tx
            .send(Event::Result {
                host: host.to_string(),
                index,
                label: item.label.clone(),
                outcome,
                result: item.result.clone(),
                dump: Dump::No,
                show: true,
                counts: !is_loop,
                censored,
                delegate: None,
            })
            .await;
    }
    if is_loop {
        let empty = shown.is_empty() && nothing_asked;
        if empty || any_failed {
            let results: Vec<(Option<Value>, TaskResult)> = shown
                .iter()
                .map(|s| (s.element.clone(), s.result.clone()))
                .collect();
            let aggregate = if empty {
                empty_loop_result()
            } else {
                match registered_value(task, &results) {
                    Value::Object(mut m) => {
                        m.remove("results");
                        TaskResult(m)
                    }
                    _ => TaskResult::default(),
                }
            };
            let outcome = classify(&aggregate, !unignored, rescuable);
            if any_failed {
                failure = Some(aggregate.clone());
            }
            let _ = tx
                .send(Event::Result {
                    host: host.to_string(),
                    index,
                    label: None,
                    outcome,
                    result: aggregate,
                    dump: Dump::No,
                    show: empty,
                    counts: true,
                    censored,
                    delegate: None,
                })
                .await;
        }
    }
    if unignored {
        failure.or_else(|| Some(TaskResult::default()))
    } else {
        None
    }
}
