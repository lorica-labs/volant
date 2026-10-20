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
use super::prepare::{PlayPlan, Prepared, hide_after_failure, pause_of, prepare, resolve};
use super::run::{break_after, classify, empty_loop_result, loops, registered_value};

mod request;
use request::include_request;

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
            | Prepared::Local(items, _)
            | Prepared::Remote(items, _, _, _, _, _),
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
                    result: super::prepare::failed_result(&err),
                    failed: true,
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
            ignored: None,
        }];
        return (Vec::new(), shown, std::time::Duration::ZERO, copy);
    }
    let step = copy.as_deref().unwrap_or(step);
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
    for item in items {
        if let Some(skipped) = &item.skipped {
            let mut result = skipped.clone();
            broke = break_after(&step.task, &item, &mut result, templar);
            let failed = result.failed();
            shown.push(Shown {
                element: item.element.clone(),
                label: item.label.clone(),
                result,
                failed,
                ignored: failed.then_some(false),
            });
            if broke {
                break;
            }
            passed += 1;
            continue;
        }
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
                    abort.raise_with(4, format!("{err:#}"));
                    return (Vec::new(), Vec::new(), std::time::Duration::ZERO, copy);
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
            // The strategy fails the host when the file cannot be loaded, and `ignore_errors`
            // belongs to the executor: measured on ansible-core 2.19.12, a missing file or a
            // mapping instead of a list ends the host at exit 2 whatever the keyword says.
            ignored: Some(false),
        });
    }
    let owed = if broke {
        passed
    } else {
        passed.saturating_sub(1)
    };
    (groups, shown, pause.unwrap_or_default() * owed, copy)
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
                error: (item.failed && item.result.0.contains_key(crate::render::CHAIN_KEY)).then(
                    || crate::render::ErrorBlock::of(&item.result, task.origin.as_ref(), censored),
                ),
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
                    show: skipped,
                    counts: true,
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
