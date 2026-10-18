// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeSet;

use serde_json::{Value, json};
use volant_protocol::TaskResult;
use volant_protocol::modules::short_name;

use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::template::Templar;
use crate::vars::VarStore;

use super::local::set_fact_data;
use super::register::registered_value;

/// The hosts a step's registered variable and facts are written for: every live host of the
/// batch when one of them runs the step for all of them, and the step's own host otherwise.
///
/// Measured on ansible-core 2.19.12: a `run_once` task's `register` and its `set_fact` are both
/// readable on every host of the batch, not only on the one that ran it.
/// Writes a task's `register` name for every host the task writes for.
///
/// **The one place a task's result becomes a variable**, and it writes untrusted whatever the
/// module was: native, controller-side, or a Python module whose payload the agent ran. A
/// result is a managed host's own words, so a template inside one is text and never a template
/// again - the rule the trust model rests on since the unconditional "a `set_fact` is data"
/// rule was dropped, which leaves nothing under a site that writes a result the other way.
///
/// A task with no `register` writes nothing, which is why the check lives here rather than at
/// each caller: a caller that forgets it writes nothing instead of writing under an empty name.
///
/// `warnings` are the ones [`super::register::take_warnings`] took off the results. Measured on ansible-core
/// 2.19.12 with a module calling `module.warn`: the registered result keeps them in `warnings`,
/// and a loop's keeps every item's there, in item order, with none left on the items
/// (`TaskExecutor._run_loop` gathers them, `_execute` puts back what its warning context
/// captured). `pause` warns through `display.warning` instead, and registers no such key.
pub(in crate::executor) fn record_registered(
    vars: &mut VarStore,
    task: &PlayTask,
    targets: &[String],
    results: &[(Option<Value>, TaskResult)],
    warnings: &[String],
) {
    let Some(reg) = &task.register else {
        return;
    };
    let mut value = registered_value(task, results);
    // The reference registers a module's result without its `invocation` (`_process_pending_results`
    // deletes the top-level key, and only that one: a loop's items keep theirs), so a playbook
    // reading `r.invocation` fails there. `-vvv` still shows it, from the result itself.
    if let Value::Object(map) = &mut value {
        map.remove("invocation");
        if !warnings.is_empty() && short_name(&task.module) != "pause" {
            map.insert("warnings".into(), json!(warnings));
        }
    }
    for target in targets {
        vars.set_untrusted_fact(target, reg, value.clone());
    }
}

/// Writes the `ansible_facts` of a batch's results for every host the batch writes for.
///
/// **The one place a result's facts become variables**, beside [`record_registered`] and for the
/// same reason: a second road in is a road that writes them the trusted way. It is asked of every
/// remote result and not of `setup` alone, because that is where the facts are - `package_facts`,
/// `getent` and a module returning `ansible_facts` of its own all answer in the same field.
///
/// Not asked of a controller-side result. `set_fact` and `include_vars` write their own facts as
/// they run, and they alone know which of them a host wrote and which the playbook did: rewriting
/// them here would give an author's `set_fact` a managed host's trust and lose the one distinction
/// `run_local` exists to keep.
///
/// Returns the restricted names [`VarStore::gather_facts`] took out and has not warned about
/// yet in this run, for the caller to warn about.
///
/// A task that failed writes nothing, as in the reference, which records facts only for a task
/// that did not fail, `ignore_errors` or not. `results` have been through `failed_when`, and a
/// loop is judged whole: one failed item fails the task.
pub(in crate::executor) fn record_facts(
    vars: &mut VarStore,
    targets: &[String],
    results: &[(Option<Value>, TaskResult)],
) -> Vec<String> {
    let mut removed = Vec::new();
    if results.iter().any(|(_, result)| result.failed()) {
        return removed;
    }
    for (_, result) in results {
        let Some(facts) = result.0.get("ansible_facts").and_then(Value::as_object) else {
            continue;
        };
        for target in targets {
            removed.extend(vars.gather_facts(target, facts));
        }
    }
    removed
}

/// The key `set_stats` answers under: not a host setting, so not read through `host_setting`.
const STATS_KEY: &str = "ansible_stats";

/// Applies what `add_host`, `group_by` and `set_stats` answered for `host`, once the task is
/// judged: the inventory gains the host or the group, the custom stats gain the data.
///
/// Keyed on the action's name and never on the keys alone: a managed host's module result can
/// carry any key, and `add_host` among them is a restricted one that is taken out of its facts.
/// A failed task, or one of its loop items, applies nothing, as in the reference.
pub(in crate::executor) fn record_inventory_results(
    vars: &mut VarStore,
    task: &PlayTask,
    host: &str,
    results: &[(Option<Value>, TaskResult)],
) {
    let kind = short_name(&task.module);
    if !["add_host", "group_by", "set_stats"].contains(&kind)
        || results.iter().any(|(_, r)| r.failed())
    {
        return;
    }
    let names = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };
    for (_, result) in results.iter().filter(|(_, r)| !r.skipped()) {
        match kind {
            "add_host" => {
                let Some(added) = result.0.get("add_host").and_then(Value::as_object) else {
                    continue;
                };
                let Some(name) = added.get("host_name").and_then(Value::as_str) else {
                    continue;
                };
                let host_vars = added
                    .get("host_vars")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                vars.add_host(name, &names(added.get("groups")), &host_vars);
            }
            "group_by" => {
                if let Some(group) = result.0.get("add_group").and_then(Value::as_str) {
                    vars.add_to_group(host, group, &names(result.0.get("parent_groups")));
                }
            }
            _ => {
                if let Some(stats) = result.0.get(STATS_KEY) {
                    vars.record_custom_stats(host, stats);
                }
            }
        }
    }
}

/// Writes the facts a controller-side `set_fact` or `include_vars` answered, once `failed_when`
/// has judged the task. A failed task writes none, `ignore_errors` or not, and a loop is judged
/// whole, as [`record_facts`] judges a host's: measured on ansible-core 2.19.12, a `set_fact` or
/// an `include_vars` under `failed_when: true` and `ignore_errors: true` leaves its names
/// undefined at the next task, and so does a `set_fact` loop whose last item failed.
///
/// Only the `set_fact` values whose own render read a managed host are data. Measured on
/// ansible-core 2.19.12: a fact the playbook wrote can still name the variable a `debug: var:`
/// shows, so writing every fact as data would fail a play with no host value anywhere in it.
pub(in crate::executor) fn record_local_facts(
    vars: &mut VarStore,
    task: &PlayTask,
    items: &[Item],
    hosts: &[String],
    results: &[(Option<Value>, TaskResult)],
    templar: &Templar,
) {
    let set_fact = match short_name(&task.module) {
        "set_fact" => true,
        "include_vars" => false,
        _ => return,
    };
    if results.iter().any(|(_, r)| r.failed()) {
        return;
    }
    let mut written = BTreeSet::new();
    for (item, (_, result)) in items.iter().zip(results) {
        let Some(facts) = result.0.get("ansible_facts").and_then(Value::as_object) else {
            continue;
        };
        let data = set_fact_data(task, item, templar);
        let cacheable = set_fact
            && item.args.get("cacheable").is_some_and(|v| match v {
                Value::Bool(on) => *on,
                Value::String(text) => crate::yaml::bool_from_str(text.trim()) == Some(true),
                _ => false,
            });
        for (key, value) in facts {
            for host in hosts {
                if cacheable {
                    vars.cache_fact(host, key, value);
                    written.insert(host.clone());
                }
                if set_fact && data.contains(key) {
                    vars.set_untrusted_fact(host, key, value.clone());
                } else {
                    vars.set_fact(host, key, value.clone());
                }
            }
        }
    }
    // One file write per host for the whole task, however many names it set.
    for host in written {
        vars.write_cache(&host);
    }
}

pub(in crate::executor) fn fact_targets(
    task: &PlayTask,
    host: &str,
    live: &[String],
) -> Vec<String> {
    if task.runs_once() && !live.is_empty() {
        live.to_vec()
    } else {
        vec![host.to_string()]
    }
}
