// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Map, Value, json};
use volant_protocol::modules::short_name;
use volant_protocol::{TaskResult, failed_when_decides};

use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::template::{Templar, TemplateError};
use crate::vars::HostVars;

use super::argspec::refusal;
use super::local::set_fact_data;
use super::output::{CAUSE_KEY, RAISED_SKIP, brief_message, python_falsy, verbose_segments};

/// One module result, ready to report: `changed_when` and `failed_when` decide its outcome
/// wherever the module ran, controller side as well as on the agent.
pub(in crate::executor) fn finish(
    task: &PlayTask,
    item: &Item,
    result: TaskResult,
    templar: &Templar,
) -> TaskResult {
    // A run that hit the `timeout` keyword is reported as it stands. In ansible-core 2.19.12 the
    // timeout is an exception raised out of the handler's run, past everything that would have
    // judged the result, so `failed_when: false` cannot make a pass of it. A refused raw
    // argument is raised before the handler runs, and stands the same way.
    if timed_out(&result) || refusal(task).is_some() {
        return result;
    }
    let mut r = apply_conditions(task, item, result, templar);
    settle_failure(&mut r, task);
    r
}

/// Whether a task's results fail it, a loop's items included: what its facts, its notified
/// handlers and the inventory it changed wait on. [`TaskResult::is_failed`] reads the result, so
/// a task whose retries ran out under `failed_when: false` passes.
pub(in crate::executor) fn task_fails(results: &[(Option<Value>, TaskResult)]) -> bool {
    let decided = failed_when_decides(results.iter().map(|(_, r)| r));
    results.iter().any(|(_, r)| r.is_failed_among(decided))
}

/// Whether a result is the one [`TaskResult::timed_out`] builds: a failure carrying the period
/// it ran out of.
pub(super) fn timed_out(result: &TaskResult) -> bool {
    result.failed()
        && result
            .0
            .get("timedout")
            .and_then(|t| t.get("period"))
            .is_some()
}

/// Applies `changed_when` and `failed_when` to one result, with `result` bound to it.
///
/// First, `failed` is settled: filled in when the result does not carry it, with the value
/// [`TaskResult::failed`] reads off `rc`, and made `true` on a result that fails by a truthy
/// non-boolean. ansible-core's `TaskExecutor._execute` fills it in on every result that ran,
/// before the conditions and before `register`, and registers `True` for a failure. A skipped
/// item never comes here, and the reference leaves `failed` off it too.
pub(super) fn apply_conditions(
    task: &PlayTask,
    item: &Item,
    mut result: TaskResult,
    templar: &Templar,
) -> TaskResult {
    // A skip an action plugin raised ends `_execute` before `failed` is filled in.
    if result.0.remove(RAISED_SKIP).is_some() {
        return result;
    }
    result.settle_failed();
    // `_execute` judges a result with the conditions only when it is not a skip, as check mode
    // makes of a command it will not run.
    if result.0.contains_key("skipped")
        || (task.changed_when.is_empty() && task.failed_when.is_empty())
    {
        return result;
    }
    let vars = condition_vars(task, item, &result, templar);
    if !task.changed_when.is_empty() {
        match all_hold(&task.changed_when, &vars, templar) {
            Ok(changed) => result.0.insert("changed".into(), json!(changed)),
            Err(e) => return unevaluable(result, "changed", &e),
        };
    }
    if !task.failed_when.is_empty() {
        let failed = match all_hold(&task.failed_when, &vars, templar) {
            Ok(failed) => failed,
            Err(e) => return unevaluable(result, "failed", &e),
        };
        result.0.insert("failed_when_result".into(), json!(failed));
        // Over a non-zero rc too: the condition has spoken.
        result.0.insert("failed".into(), json!(failed));
        // Measured on ansible-core 2.19.12: `command: "false"` under `failed_when: false`
        // registers `failed_when_suppressed_exception: "(traceback unavailable)"` and no
        // `exception`.
        if !failed && let Some(exception) = result.0.remove("exception") {
            result
                .0
                .insert("failed_when_suppressed_exception".into(), exception);
        }
    }
    result
}

/// A result whose `changed_when` or `failed_when` could not be evaluated, as ansible-core's
/// `_execute` leaves it: every key the module returned kept, `failed` set, and the error under
/// `changed_when_result` or `failed_when_result`, the conditions after it not evaluated. Its `msg`
/// is then [`settle_failure`]'s. Measured on ansible-core 2.19.12: `command: "true"` under
/// `failed_when: nosuch == 1` registers `failed_when_result: "Error while evaluating conditional:
/// 'nosuch' is undefined"` beside its `rc` and its empty `msg`; `changed_when: a.nokey` registers
/// `changed_when_result` and never reads a `failed_when: true` behind it; a syntax error reads
/// `Syntax error in template: unexpected '*'`. The prefixes are the reference's; what follows them
/// is this engine's wording of the same fault.
fn unevaluable(mut result: TaskResult, condition: &str, err: &TemplateError) -> TaskResult {
    let text = match err.0.strip_prefix("syntax error: ") {
        Some(rest) => format!("Syntax error in template: {rest}"),
        None => format!("Error while evaluating conditional: {}", err.0),
    };
    result.0.insert("failed".into(), json!(true));
    result
        .0
        .insert(format!("{condition}_when_result"), json!(text));
    result
}

/// Whether the loop ends after this item: every `break_when` condition holds against the item's
/// variables with the result under the registered name, the way the reference evaluates it
/// right after the item. A condition that cannot be evaluated fails the item instead, and the
/// loop ends there too.
pub(in crate::executor) fn break_after(
    task: &PlayTask,
    item: &Item,
    result: &mut TaskResult,
    templar: &Templar,
) -> bool {
    if task.break_when.is_empty() {
        return false;
    }
    let mut vars = item.vars.clone();
    // The reference evaluates `break_when` against the loop's variables once the item's facts
    // are in them, `delegate_facts` or not.
    lay_returned_facts(&mut vars, task, item, result, templar, false);
    if let Some(name) = &task.register {
        // What the host returned: data, never templated again.
        vars.insert_untrusted(name.clone(), Value::Object(result.0.clone()));
    }
    for condition in &task.break_when {
        match templar.condition(condition, &vars) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(err) => {
                *result = unevaluable(std::mem::take(result), "break", &err);
                return true;
            }
        }
    }
    true
}

/// Walks the items `from` up to `upto` and says which one `break_when` ends the loop at, if any,
/// and how many items it passed without one. A skipped item counts, as in the reference; an item
/// that never ran has no result and is passed. The reference pauses after every item that does
/// not end the loop and has another behind it, so the caller owes one pause per item passed,
/// less one when `upto` is the end of the loop.
pub(in crate::executor) fn broke_before(
    task: &PlayTask,
    items: &[Item],
    received: &mut [Option<TaskResult>],
    from: &mut usize,
    upto: usize,
    templar: &Templar,
) -> (Option<usize>, usize) {
    let mut passed = 0;
    while *from < upto {
        let ii = *from;
        *from += 1;
        let mut result = match (&items[ii].skipped, &received[ii]) {
            (Some(s), _) => s.clone(),
            (None, Some(r)) => r.clone(),
            (None, None) => {
                passed += 1;
                continue;
            }
        };
        if break_after(task, &items[ii], &mut result, templar) {
            received[ii] = Some(result);
            return (Some(ii), passed);
        }
        passed += 1;
    }
    (None, passed)
}

/// The variables `changed_when`, `failed_when` and `until` read for one result: the item's, with
/// the facts the result returned laid over them, then the registered name and `result`.
///
/// ansible-core's `TaskExecutor._execute` updates its copy of the variables with
/// `result['ansible_facts']` before it judges, and registers the result again after that, so a
/// fact never hides the registered name. `set_fact` and `include_vars` write theirs as they are,
/// unless `delegate_facts` sends them to the delegate, which leaves them out of sight here; any
/// other module's go through [`HostVars::returned_facts`], `delegate_facts` or not. `debug` is
/// the one module whose facts are never read. Measured on ansible-core 2.19.12: `set_fact:
/// {seen: 1}` passes `failed_when: seen is not defined`, a module's fact wins over a `-e` of the
/// same name, and a delegated `set_fact` under `delegate_facts: true` does not see its own fact.
pub(super) fn condition_vars(
    task: &PlayTask,
    item: &Item,
    result: &TaskResult,
    templar: &Templar,
) -> HostVars {
    let mut vars = item.vars.clone();
    let delegated =
        task.delegate_to.as_deref().is_some_and(|to| !to.is_empty()) && task.delegates_facts();
    lay_returned_facts(&mut vars, task, item, result, templar, delegated);
    if let Some(reg) = &task.register {
        vars.insert_untrusted(reg.clone(), Value::Object(result.0.clone()));
    }
    vars.insert_untrusted("result".into(), Value::Object(result.0.clone()));
    vars
}

/// Lays the `ansible_facts` a result returned over `vars`. `hidden` leaves out the facts of a
/// `set_fact` or `include_vars` that `delegate_facts` sends to the delegate.
fn lay_returned_facts(
    vars: &mut HostVars,
    task: &PlayTask,
    item: &Item,
    result: &TaskResult,
    templar: &Templar,
    hidden: bool,
) {
    let Some(facts) = result.0.get("ansible_facts").and_then(Value::as_object) else {
        return;
    };
    match short_name(&task.module) {
        "debug" => {}
        "set_fact" | "include_vars" if hidden => {}
        // The trust `record_local_facts` gives the same names once they are written.
        name @ ("set_fact" | "include_vars") => {
            let data = set_fact_data(task, item, templar);
            for (key, value) in facts {
                if name == "set_fact" && data.contains(key) {
                    vars.insert_untrusted(key.clone(), value.clone());
                } else {
                    vars.insert(key.clone(), value.clone());
                }
            }
        }
        _ => vars.returned_facts(facts),
    }
}

/// What ansible-core's `maybe_raise_on_result` leaves on a failed final result, once the
/// conditions and `until` are done with it: `exception: "(traceback unavailable)"`, and a `msg`
/// when the result carries none. Measured on ansible-core 2.19.12: a `set_fact`, a `ping` and a
/// `ping` loop item under `failed_when: true`, and a `ping` whose `until: false` ran out, all
/// register `msg: "Task failed: Action failed: Unknown error."`; a `debug` keeps its own `msg`
/// and a `command` its empty one. `until` never sees that `msg`: `until: r.msg is defined` over
/// `failed_when: true` retries. A module result that already carried a truthy `exception` was
/// normalised as the module's error, and reads `Module failed` instead; a controller-side action
/// (`fail`, `assert`) is the action's own error whatever it carries, measured too.
pub(super) fn settle_failure(result: &mut TaskResult, task: &PlayTask) {
    let cause = result.0.remove(CAUSE_KEY);
    if result.failed() {
        let local = volant_protocol::modules::local(&task.module).is_some();
        let by = match result.0.get("exception") {
            Some(e) if !local && !python_falsy(e) => "Module",
            _ => "Action",
        };
        // A module that raised sends no `msg`, only the summary of its exception, which the
        // reference words into the result's `msg` (`result_dict_from_exception`).
        let by_failed = format!("{by} failed.");
        let brief = cause.as_ref().and_then(brief_message);
        let raised = brief.as_ref().map(|brief| {
            crate::render::task_failed(&crate::render::concat_message(&by_failed, brief))
        });
        result.0.entry("msg").or_insert_with(|| {
            json!(raised.unwrap_or_else(|| format!("Task failed: {by} failed: Unknown error.")))
        });
        // The block the reference prints over the failure's line: `Task failed: <by> failed: `
        // and the error's brief message, the module's summary when it sent one.
        let msg = match result.0.get("msg") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => crate::render::ansible_json(other),
            None => String::new(),
        };
        let detail = brief.unwrap_or_else(|| msg.clone());
        // `AnsibleError`: an empty message is the default one alone, `Action failed.`.
        // `Unexpected AnsibleActionFail error: ...` is the sentence the reference's own `copy`
        // fails with for a source that is not there, and its block carries it as it is. Any
        // other result starting with it only loses the `Action failed: ` in its error block.
        let message = if msg.starts_with("Task failed: ") {
            msg
        } else if msg.starts_with("Unexpected AnsibleActionFail error: ") {
            crate::render::task_failed(&msg)
        } else if detail.is_empty() {
            crate::render::task_failed(&by_failed)
        } else {
            crate::render::task_failed(&crate::render::concat_message(&by_failed, &detail))
        };
        // The causes the module's summary chains: the first link says the task and the module
        // failed and gives the module's own message, the others follow behind `caused by`.
        if let Some(mut links) = cause.as_ref().and_then(verbose_segments)
            && links.len() > 1
            && !result.0.contains_key(crate::render::CHAIN_KEY)
        {
            let lead = links.remove(0);
            let lead =
                crate::render::task_failed(&crate::render::concat_message(&by_failed, &lead));
            let mut chain = vec![crate::render::lead(&lead)];
            chain.extend(
                links
                    .iter()
                    .map(|link| crate::render::cause(link, None, None)),
            );
            result
                .0
                .insert(crate::render::CHAIN_KEY.into(), Value::Array(chain));
        }
        // A failure raised before the action ran already carries its block's message.
        result
            .0
            .entry(crate::render::ERROR_KEY)
            .or_insert_with(|| Value::from(message));
        result
            .0
            .insert("exception".into(), json!("(traceback unavailable)"));
    }
}

pub(super) fn all_hold(
    conditions: &[String],
    vars: &HostVars,
    templar: &Templar,
) -> Result<bool, TemplateError> {
    for c in conditions {
        if !templar.condition(c, vars)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// What a loop over an empty list reports and registers. The reference shows the task as
/// `skipping` and gives `skipped_reason` this exact wording, with no items behind it.
pub(in crate::executor) fn empty_loop_result() -> TaskResult {
    let mut r = Map::new();
    r.insert("changed".into(), json!(false));
    r.insert("skipped".into(), json!(true));
    r.insert("skipped_reason".into(), json!("No items in the list"));
    TaskResult(r)
}

/// Whether a task's results are a loop's, given its first item's element. A loop whose list was
/// an undefined variable and whose `when` left it out has one item with no element, and it is
/// reported and registered as a plain task: the reference's `items` is `None` there, so the task
/// goes through `_execute` once, never through `_run_loop`.
pub(in crate::executor) fn loops(task: &PlayTask, first: Option<&Option<Value>>) -> bool {
    task.loop_items.is_some() && !matches!(first, Some(None))
}
