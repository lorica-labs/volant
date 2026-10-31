// SPDX-License-Identifier: GPL-3.0-or-later
//! Resolving a dynamic `include_tasks`, `import_tasks` or `include_role` statement for one host.

use std::sync::Mutex;

use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use volant_protocol::TaskResult;

use crate::compile::{Compiled, IncludeKind, IncludeParams, IncludeTarget, Step};
use crate::playbook::PlayTask;
use crate::render::Dump;
use crate::stats::Outcome;
use crate::template::Templar;
use crate::transport::ConnectionDefaults;
use crate::vars::VarStore;

use super::coordinator::{Event, Progress};
use super::prepare::{
    PlayPlan, Prepared, hide_after_failure, pause_of, prepare, render_deferred, resolve,
};
use super::run::{break_after, classify, empty_loop_result, loops, registered_value};

mod request;
mod shown;
use request::include_request;
pub(super) use shown::NO_FAILED_VARS;
use shown::{Shown, item_error};

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
    /// For a role, which instance it is, and whether the statement said `allow_duplicates:
    /// false`. The coordinator keeps what each host has run, so it decides which hosts the
    /// expansion is spliced in for.
    pub(super) role_id: Option<String>,
    pub(super) unique: bool,
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
#[expect(clippy::too_many_lines)]
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
    abort: &super::Abort,
) -> (
    Vec<IncludeGroup>,
    Vec<Shown>,
    std::time::Duration,
    Option<Box<Step>>,
) {
    let items = match prepare(step, host, plan, live, templar, store, defaults, warnings) {
        Ok(
            Prepared::Skipped(items)
            | Prepared::Local(items, ..)
            | Prepared::Remote(items, _, _, _, _, _),
        ) => items,
        // A `when` that cannot be evaluated or a `loop` that is not a list, reported with the
        // reference's own prefix for a task that dies before it runs. Not deferred the way an
        // ordinary task's is: this arm is reached with the batch empty, so there is nothing to
        // send out first.
        Err(err) => {
            let result = super::prepare::failed_result(&err);
            // Registered all the same: measured on ansible-core 2.19.12, `register` on an
            // include whose `when` fails holds a failed result.
            super::run::record_registered(
                &mut store.lock().expect("vars lock"),
                &step.task,
                &super::run::fact_targets(&step.task, host, &live.live_hosts),
                &[(None, result.clone())],
                &[],
            );
            return (
                Vec::new(),
                vec![Shown {
                    element: None,
                    label: None,
                    result,
                    failed: true,
                    load_failed: false,
                    ignored: None,
                }],
                std::time::Duration::ZERO,
                hide_after_failure(step, host, plan, live, templar, store),
            );
        }
    };
    // The statement's own keywords that are templates, rendered for this host: its `no_log`
    // censors the `included:` line, and a value the reference cannot take fails the statement.
    let ready: Vec<&crate::vars::HostVars> = items
        .iter()
        .filter(|i| i.skipped.is_none())
        .map(|i| &i.vars)
        .collect();
    let resolution = resolve(step, &ready, templar);
    warnings.extend(resolution.warnings);
    let copy = resolution.step;
    if let Some(err) = resolution.error {
        let shown = vec![Shown {
            element: None,
            label: None,
            result: TaskResult::failed_with(super::prepare::failure_text(&err)),
            failed: true,
            load_failed: false,
            ignored: None,
        }];
        return (Vec::new(), shown, std::time::Duration::ZERO, copy);
    }
    let step = copy.as_deref().unwrap_or(step);
    let is_loop = loops(&step.task, items.first().map(|i| &i.element));
    // What `register` holds: an entry per item, the loop's keys added later, whether the file
    // could be loaded or not. Measured on ansible-core 2.19.12.
    let mut registered = Vec::new();
    // `break_when` ends an include loop after the item it holds for, a skipped one included, and
    // `pause` waits after every other item that has another behind it: measured on ansible-core
    // 2.19.12, `include_tasks` over `[1, 2, 3]` with `break_when: item == 2` includes 1 and 2.
    // The wait is returned for the driver to take, which can be interrupted; nothing is shown in
    // the meantime, so the order is the same.
    let pause = pause_of(&step.task, &items, templar);
    let mut passed = 0_u32;
    let mut broke = false;
    let mut groups = Vec::new();
    let mut shown = Vec::new();
    for (ii, mut item) in items.into_iter().enumerate() {
        warnings.extend(render_deferred(
            &step.task, plan, templar, host, &mut item, ii,
        ));
        if let Some(skipped) = &item.skipped {
            let mut result = skipped.clone();
            broke = break_after(&step.task, &item, &mut result, templar);
            registered.push((item.element.clone(), result.clone()));
            let failed = result.failed();
            shown.push(Shown {
                element: item.element.clone(),
                label: item.label.clone(),
                result,
                failed,
                load_failed: false,
                ignored: failed.then_some(false),
            });
            if broke {
                break;
            }
            passed += 1;
            continue;
        }
        let entry = registered_entry(&item, matches!(kind, IncludeKind::Role), is_loop);
        let load_failed;
        let result = match include_request(compiled, step, kind, &item, templar) {
            // The file is there and cannot be used: it holds a mapping rather than a list of
            // tasks, its YAML does not parse, or something inside it names a module, an option
            // or a keyword this release refuses. Measured on ansible-core 2.19.12 for the first
            // of those:
            // `fatal: [h1]: FAILED! => {"changed": false, "include": "mapping.yml", "reason":
            // "included task files must contain a list of tasks"}`, exit 2, with a recap.
            Ok(request) => match crate::compile::expand_include(compiled, step, &request).and_then(
                |mut expanded| {
                    crate::preflight::check_built(&expanded, plan.python.as_deref())?;
                    // The lookups of its `with_` loops, which the reference resolves when it
                    // loads the file: a name that is none fails the include, not a task of it.
                    plan.steps()
                        .resolved
                        .ensure(
                            crate::collection::tasks_of(&expanded).collect::<Vec<_>>(),
                            templar.bridge().map(|bridge| &**bridge),
                        )
                        .map_err(|e| anyhow::anyhow!(e))?;
                    if plan.strategy.free() {
                        warnings.extend(super::free::loosen(&mut expanded));
                    }
                    Ok(expanded)
                },
            ) {
                Ok(expanded) => {
                    registered.push((item.element.clone(), entry.clone()));
                    let mut included = TaskResult(Map::from_iter([
                        ("changed".to_string(), json!(false)),
                        ("include".to_string(), json!(request.what)),
                    ]));
                    broke = break_after(&step.task, &item, &mut included, templar);
                    if included.failed() {
                        // A `break_when` that cannot be evaluated fails the item.
                        shown.push(Shown {
                            element: item.element,
                            label: item.label,
                            result: included,
                            failed: true,
                            load_failed: false,
                            ignored: Some(false),
                        });
                        break;
                    }
                    groups.push(IncludeGroup {
                        key: request.key(),
                        unique: matches!(&request.target, IncludeTarget::Role(e) if e.unique),
                        role_id: request.role_id,
                        what: request.what,
                        label: request.label,
                        params: request.vars,
                        expanded,
                    });
                    if broke {
                        break;
                    }
                    passed += 1;
                    continue;
                }
                // A file the reference cannot parse ends the run at exit 4, after the
                // statement's banner and without a recap: measured on ansible-core 2.19.12.
                Err(err)
                    if err
                        .downcast_ref::<crate::playbook::TaskFileRefused>()
                        .is_some() =>
                {
                    let refused = err
                        .downcast_ref::<crate::playbook::TaskFileRefused>()
                        .expect("checked above");
                    if refused.1 {
                        abort.raise_plain_with(4, refused.0.clone());
                    } else {
                        abort.raise_with(4, format!("{err:#}"));
                    }
                    return (Vec::new(), Vec::new(), std::time::Duration::ZERO, copy);
                }
                Err(err) => {
                    registered.push((item.element.clone(), entry.clone()));
                    load_failed = true;
                    let mut body = Map::new();
                    body.insert("changed".into(), json!(false));
                    body.insert("include".into(), json!(request.what));
                    body.insert("reason".into(), json!(load_reason(&format!("{err:#}"))));
                    TaskResult(body)
                }
            },
            Err(result) => {
                // A file that is not there is a file that could not be loaded.
                load_failed = result.0.contains_key("reason");
                registered.push((
                    item.element.clone(),
                    if load_failed { entry } else { result.clone() },
                ));
                result
            }
        };
        shown.push(Shown {
            element: item.element,
            label: item.label,
            result,
            failed: true,
            // The strategy fails the host when the file cannot be loaded, and `ignore_errors`
            // belongs to the executor: measured on ansible-core 2.19.12, a missing file or a
            // mapping instead of a list ends the host at exit 2 whatever the keyword says.
            load_failed,
            ignored: Some(false),
        });
    }
    let owed = if broke {
        passed
    } else {
        passed.saturating_sub(1)
    };
    // A loop's index variable is in each entry, as a module's result has it.
    if let (true, Some(name)) = (is_loop, &step.task.index_var) {
        for (index, (_, entry)) in registered.iter_mut().enumerate() {
            entry.0.insert(name.clone(), json!(index));
        }
    }
    super::run::record_registered(
        &mut store.lock().expect("vars lock"),
        &step.task,
        &super::run::fact_targets(&step.task, host, &live.live_hosts),
        &registered,
        &[],
    );
    (groups, shown, pause.unwrap_or_default() * owed, copy)
}

/// One item's entry in what `register` holds. Measured on ansible-core 2.19.12: `include` names
/// the file as written (a role has none), `include_args` holds the statement's other arguments,
/// and a statement with no loop adds `changed: false`.
fn registered_entry(item: &super::prepare::Item, role: bool, is_loop: bool) -> TaskResult {
    let mut args = item.args.clone();
    let written = if role {
        None
    } else {
        let raw = args.remove("_raw_params");
        args.remove("file").or(raw)
    };
    let mut entry = Map::new();
    if !is_loop {
        entry.insert("changed".into(), json!(false));
    }
    if let Some(written) = written {
        entry.insert("include".into(), written);
    }
    entry.insert("include_args".into(), Value::Object(args));
    TaskResult(entry)
}

/// Sends the lines an include step shows and its `TaskDone`, and answers with the result a rescue
/// would be given when it failed for good.
///
/// It is not [`super::report::report_task`] because of what an include **does not** count.
/// Measured on ansible-core 2.19.12: a statement that brought something in counts one `ok` per
/// host per item, and that one is counted by the coordinator when it prints the `included:`
/// line - so the aggregate a looping statement would otherwise contribute has to count nothing,
/// or a two-item loop would read three. The aggregates that do count are the ones with no
/// `included:` line behind them: a loop over an empty list or whose every item was skipped,
/// which shows `skipping:` and counts it, and a loop an item failed in, which counts the failure
/// without a line of its own.
pub(super) async fn report_include(
    tx: &mpsc::Sender<Event>,
    host: &str,
    index: usize,
    task: &PlayTask,
    shown: &[Shown],
    rescuable: bool,
    nothing_asked: bool,
) -> Option<TaskResult> {
    let is_loop = loops(task, shown.first().map(|s| &s.element));
    let censored = task.censors();
    let mut any_failed = false;
    // A failure no item's `ignore_errors` swallowed: that is what fails the host.
    let mut unignored = false;
    let mut failure: Option<TaskResult> = None;
    for item in shown {
        let ignored = item.ignored.unwrap_or_else(|| task.ignores_errors());
        unignored |= item.failed && !ignored;
        let (outcome, result) = item.outcome(ignored, rescuable);
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
                result,
                dump: Dump::No,
                // Measured on ansible-core 2.19.12: a file a loop item could not load prints its
                // `[ERROR]` block and no line, and the host fails all the same.
                show: !(item.load_failed && is_loop),
                counts: !is_loop || item.load_failed,
                censored,
                delegate: None,
                error: item_error(item, task, censored),
                task_origin: task.origin.clone(),
            })
            .await;
    }
    if is_loop {
        let empty = shown.is_empty() && nothing_asked;
        // Every item skipped, including the case of no item at all. Measured on ansible-core
        // 2.19.12: the strategy counts a result `is_skipped()` before it looks at what the task
        // was, and a loop's result is skipped when every item's is, so a looped include whose
        // items all skip shows `skipping: [h]` after its item lines and counts `skipped=1`.
        let skipped = nothing_asked && !any_failed;
        if skipped || any_failed {
            let results: Vec<(Option<Value>, TaskResult)> = shown
                .iter()
                .map(|s| (s.element.clone(), s.result.clone()))
                .collect();
            let mut aggregate = if empty {
                empty_loop_result()
            } else {
                match registered_value(task, &results) {
                    Value::Object(mut m) => {
                        if let Some(items) = m.remove("results") {
                            m.insert(crate::render::LOOP_RESULTS.into(), items);
                        }
                        TaskResult(m)
                    }
                    _ => TaskResult::default(),
                }
            };
            // A file a loop item could not load fails the host from the strategy, not the task:
            // its items counted it, and the rescue reads no `ansible_failed_result`.
            let unloaded = shown.iter().any(|s| s.load_failed);
            if any_failed {
                aggregate.0.insert("failed".into(), json!(true));
            }
            let outcome = if unloaded && unignored {
                Outcome::Failed
            } else {
                classify(&aggregate, !unignored, rescuable)
            };
            if any_failed {
                let mut failed = aggregate.clone();
                if unloaded {
                    failed.0.insert(NO_FAILED_VARS.into(), json!(true));
                }
                failure = Some(failed);
            }
            let _ = tx
                .send(Event::Result {
                    host: host.to_string(),
                    index,
                    label: None,
                    outcome,
                    result: aggregate,
                    dump: Dump::No,
                    show: skipped,
                    counts: !unloaded,
                    censored,
                    delegate: None,
                    error: None,
                    task_origin: None,
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

/// Why a file an include brings in could not be loaded. A file it imports that is not there is
/// named once more in front, as the reference's loader wraps it: measured on ansible-core 2.19.12,
/// `Could not find or access '<path>' on the Ansible Controller: Unable to retrieve file
/// contents.` and the rest.
fn load_reason(text: &str) -> String {
    let text = text.trim_end();
    let missing = text
        .strip_prefix("Unable to retrieve file contents.\nCould not find or access '")
        .and_then(|rest| rest.split_once("' on the Ansible Controller."));
    match missing {
        Some((path, _)) => {
            format!("Could not find or access '{path}' on the Ansible Controller: {text}")
        }
        None => text.to_string(),
    }
}
