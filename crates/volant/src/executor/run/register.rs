// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use crate::compile::Compiled;
use crate::executor::prepare::{Item, display};
use crate::playbook::PlayTask;
use crate::template::Templar;

use super::judge::{empty_loop_result, loops};

/// What `register` stores: the single result, or Ansible's loop aggregate.
pub(in crate::executor) fn registered_value(
    task: &PlayTask,
    results: &[(Option<Value>, TaskResult)],
) -> Value {
    if !loops(task, results.first().map(|(element, _)| element)) {
        return results.first().map_or(Value::Null, |(_, r)| {
            Value::Object(crate::executor::report::without_error(r).0)
        });
    }
    if results.is_empty() {
        let mut agg = empty_loop_result().0;
        agg.insert("results".into(), Value::Array(Vec::new()));
        return Value::Object(agg);
    }
    let mut list = Vec::new();
    for (element, r) in results {
        let mut m = crate::executor::report::without_error(r).0;
        if let Some(el) = element {
            crate::executor::report::item_keys(task, &mut m, el);
        }
        list.push(Value::Object(m));
    }
    let mut agg = Map::new();
    agg.insert("results".into(), Value::Array(list));
    agg.insert(
        "changed".into(),
        json!(results.iter().any(|(_, r)| r.changed())),
    );
    // Measured on ansible-core 2.19.12: a loop with a failed item registers `One or more items
    // failed`, `ignore_errors` or not, and one whose every item was skipped `All items skipped`.
    let failed = results.iter().any(|(_, r)| r.failed());
    if failed {
        agg.insert("failed".into(), json!(true));
    }
    let skipped = results.iter().all(|(_, r)| r.skipped());
    agg.insert("skipped".into(), json!(skipped));
    let msg = if skipped {
        "All items skipped"
    } else if failed {
        "One or more items failed"
    } else {
        "All items completed"
    };
    agg.insert("msg".into(), json!(msg));
    Value::Object(agg)
}

/// `ansible_failed_task`: the task that failed, as a rescue reads it.
///
/// Measured on ansible-core 2.19.12 with a `command` carrying `register`, `when`, `tags` and
/// `vars`: the dictionary holds one key per task keyword, `action` holding the module's short
/// name, plus six keys of the engine's own (`uuid`, `finalized`, `squashed`, `_resolved_action`,
/// `async_val`, `loop_with`) that describe nothing a playbook wrote. Those six are deliberately
/// not reproduced, and neither are the `_ansible_*` keys the reference adds to `args` on its way
/// to the module.
///
/// The keys come from the keyword table itself rather than from a list written here, so a
/// keyword added to the grammar appears in this dictionary too. Two families are left out
/// because the reference has already resolved them by the time it builds it: `local_action`,
/// which is folded into `action`, and the `with_*` lookups, which are folded into `loop`.
///
/// A keyword this release parks reads `null` here rather than the reference's default. The
/// default would describe something nothing in this engine honours, and a rescue reading
/// `ansible_failed_task.diff` is better told nothing than told `false` by an engine that
/// never looked at the keyword. `connection`, `port` and `remote_user` read what the task, its
/// block or its play wrote, and `null` when none did.
///
/// `become_method`, `become_exe` and `become_flags` read the same way: what the task, its block
/// or its play wrote, and `null` when none did.
pub(in crate::executor) fn failed_task_value(task: &PlayTask) -> Value {
    let mut out = Map::new();
    let strings = |list: &[String]| Value::Array(list.iter().map(|s| json!(s)).collect());
    for kw in crate::keywords::TASK_KEYWORDS {
        if kw.name == "local_action" || kw.name.starts_with("with_") {
            continue;
        }
        let value = match kw.name {
            "action" => json!(short_name(&task.module)),
            "args" => Value::Object(task.args.clone()),
            "become" => json!(task.r#become),
            "become_user" => json!(task.become_user),
            "become_method" => json!(task.connect.become_method),
            "become_exe" => json!(task.connect.become_exe),
            "become_flags" => json!(task.connect.become_flags),
            "changed_when" => strings(&task.changed_when),
            "connection" => json!(task.connect.connection),
            "failed_when" => strings(&task.failed_when),
            "ignore_errors" => match &task.ignore_errors {
                Some(crate::playbook::Flag::Fixed(b)) => json!(b),
                Some(crate::playbook::Flag::Template(t)) => json!(t),
                None => Value::Null,
            },
            "loop" => task.loop_items.clone().unwrap_or(Value::Null),
            "loop_control" => json!({"loop_var": task.loop_var, "label": task.loop_label}),
            "name" => json!(task.name),
            "port" => match &task.connect.port {
                Some(port) => port
                    .parse::<u16>()
                    .map_or_else(|_| json!(port), |n| json!(n)),
                None => Value::Null,
            },
            "register" => json!(task.register),
            "remote_user" => json!(task.connect.remote_user),
            "tags" => strings(&task.tags),
            "timeout" => json!(task.timeout),
            "vars" => Value::Object(task.vars.clone()),
            "when" => strings(&task.when),
            _ => Value::Null,
        };
        out.insert(kw.name.to_string(), value);
    }
    Value::Object(out)
}

/// The handlers one finished task asks for, if it changed anything and did not fail, and the
/// warnings looking them up gave.
///
/// Measured on ansible-core 2.19.12 and each half worth stating: a task that came back `ok`
/// notifies nothing, a name notified twice runs its handler once, and for a loop it is the
/// **aggregate** that decides - one changed item is a changed task, and then every item's
/// rendered names are notified, the unchanged ones' included. A failed task notifies nothing,
/// `ignore_errors` or not: the reference files a notification from the ok branch of
/// `_process_pending_results` alone. Notifications live in the driver and nowhere else: they are
/// one host's business, and the flush the coordinator opens is the same flush whether this host
/// has anything to run in it or not.
///
/// The names come from [`Item::notify`], rendered before the task ran. A handler's `name` may be
/// a template too, rendered here against the first item's variables; `listen` never is. One that
/// does not render answers to nothing by name and warns, as the reference does, unless it
/// listens for a topic. What a host's variables render to is only ever compared with the handler
/// names, never templated again. A name no handler answers to is the `Err`: the reference raises
/// it from its strategy (`ERROR_ON_MISSING_HANDLER`, on by default), so it ends the run rather
/// than failing the task - no `ignore_errors` and no `rescue` can take it, and it is measured to
/// come after the notifying task's banner and before its result line.
pub(in crate::executor) fn notified_handlers(
    compiled: &Compiled,
    task: &PlayTask,
    items: &[Item],
    templar: &Templar,
    results: &[(Option<Value>, TaskResult)],
) -> Result<(Vec<usize>, Vec<String>), String> {
    let none = (Vec::new(), Vec::new());
    let Some(vars) = items.first().map(|i| &i.vars) else {
        return Ok(none);
    };
    if task.notify.is_empty()
        || results.iter().any(|(_, r)| r.failed())
        || !results.iter().any(|(_, r)| r.changed())
    {
        return Ok(none);
    }
    let mut warnings = Vec::new();
    let names: Vec<Option<String>> = compiled
        .handlers
        .iter()
        .map(|h| {
            let name = &h.task.name;
            if !Templar::is_template(name) {
                return Some(name.clone());
            }
            match templar.render(name, vars) {
                Ok(Value::String(rendered)) => Some(rendered),
                Ok(other) => Some(display(&other)),
                Err(e) => {
                    if h.listen.is_empty() {
                        warnings.push(format!(
                            "Handler '{name}' is unusable because it has no listen topics and the name could not be templated (host-specific variables are not supported in handler names). The error: {}",
                            e.0
                        ));
                    }
                    None
                }
            }
        })
        .collect();
    let mut notified = Vec::new();
    for name in items.iter().flat_map(|i| &i.notify) {
        let found = crate::compile::resolve_notify(compiled, name, &names);
        if found.is_empty() {
            return Err(format!(
                "The requested handler '{name}' was not found in either the main handlers list nor in the listening handlers list"
            ));
        }
        // Not deduplicated here, deliberately: what makes a handler run once is the driver
        // taking **every** copy of its index off the list when it runs it.
        notified.extend(found);
    }
    Ok((notified, warnings))
}

/// Takes the `warnings` off every result of a task, in item order, for the driver to show as
/// `[WARNING]:` lines under it and for [`super::record::record_registered`] to put back where the reference
/// registers them. Off the results, because no line the reference prints shows the key: its
/// callback pops it before it dumps a result.
pub(in crate::executor) fn take_warnings(
    results: &mut [(Option<Value>, TaskResult)],
) -> Vec<String> {
    let mut out = Vec::new();
    for (_, result) in results.iter_mut() {
        if let Some(Value::Array(list)) = result.0.remove("warnings") {
            out.extend(list.iter().map(warning_text));
        }
    }
    out
}

/// A warning as the reference registers and shows it. A string is kept as it is. A module of
/// ansible-core 2.19 answers a `WarningSummary` object, rendered as `format_event_brief_message`
/// renders it: the event's `msg`, then each cause its chain follows, joined by `concat_message`
/// (`left.rstrip(". ") + ": " + right`), a part that already ends with what follows it kept
/// alone. Anything else is shown as its JSON.
pub(super) fn warning_text(warning: &Value) -> String {
    if let Value::String(text) = warning {
        return text.clone();
    }
    let mut parts = Vec::new();
    let mut event = warning.get("event");
    while let Some(msg) = event.and_then(|e| e.get("msg")).and_then(Value::as_str) {
        parts.push(msg);
        event = event
            .and_then(|e| e.get("chain"))
            .filter(|chain| chain.get("follow") == Some(&Value::Bool(true)))
            .and_then(|chain| chain.get("event"));
    }
    let Some((deepest, outer)) = parts.split_last() else {
        return warning.to_string();
    };
    let mut message = (*deepest).to_string();
    for part in outer.iter().rev() {
        message = if part.ends_with(&message) {
            (*part).to_string()
        } else {
            format!("{}: {message}", part.trim_end_matches(['.', ' ']))
        };
    }
    message
}
