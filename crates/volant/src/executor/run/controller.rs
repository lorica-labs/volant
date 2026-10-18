// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use volant_protocol::TaskResult;
use volant_protocol::modules::{ASSERT, PAUSE};

use crate::executor::prepare::Item;
use crate::playbook::PlayTask;
use crate::preflight::PROMPT_REFUSED;
use crate::template::{Templar, TemplateError};

use super::argspec::{unchanged_failure, unsupported};
use super::output::python_falsy;
use super::retry::{PYTIME_OUT_OF_RANGE, conditional_error};

/// `assert`, measured on ansible-core 2.19.12: the conditions are evaluated in order, the first
/// false one is reported as the playbook wrote it, a string or a boolean, and an undefined name
/// fails the task the way any other conditional does.
///
/// `that` is read from the task as the playbook wrote it, never as `prepare` rendered it, and
/// each element goes through `Templar::condition`, the code `when` goes through: rendering it
/// first and evaluating the result would evaluate a value rather than the author's expression.
/// The one render is the reference's own: a `that` that is one template naming a list is that
/// list. A list a managed host wrote is data, so its strings are refused rather than compiled.
pub(super) fn assert_module(task: &PlayTask, item: &Item, templar: &Templar) -> TaskResult {
    if let Some(refusal) = unsupported(&task.module, &ASSERT, &["msg"], &item.args) {
        return unchanged_failure(refusal);
    }
    let Some(that) = task.args.get("that").filter(|v| !v.is_null()) else {
        return TaskResult::failed_with("missing required arguments: that");
    };
    let conditions = match that {
        Value::Array(list) => list.clone(),
        Value::String(s) if Templar::is_template(s) => {
            match templar.render_value_tainted(that, &item.vars) {
                Ok((Value::Array(list), from_host)) => {
                    if from_host && list.iter().any(Value::is_string) {
                        return TaskResult::failed_with(conditional_error(&TemplateError(
                            "Encountered untrusted template or expression.".into(),
                        )));
                    }
                    list
                }
                _ => vec![that.clone()],
            }
        }
        one => vec![one.clone()],
    };
    let mut r = Map::new();
    r.insert("changed".into(), json!(false));
    // `quiet` is the one thing that argument does: measured on ansible-core 2.19.12.
    if item
        .args
        .get("quiet")
        .and_then(volant_protocol::modules::arg_bool)
        != Some(true)
    {
        r.insert(crate::render::VERBOSE_ALWAYS.into(), json!(true));
    }
    for condition in &conditions {
        // A YAML boolean reaches `condition` spelled the way `when` spells one.
        let text = match condition {
            Value::String(s) => s.clone(),
            Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            other => other.to_string(),
        };
        match templar.condition(&text, &item.vars) {
            Err(e) => return TaskResult::failed_with(conditional_error(&e)),
            Ok(true) => {}
            Ok(false) => {
                let msg = item.args.get("fail_msg").or_else(|| item.args.get("msg"));
                r.insert("assertion".into(), condition.clone());
                r.insert("evaluated_to".into(), json!(false));
                r.insert("failed".into(), json!(true));
                r.insert(
                    "msg".into(),
                    msg.cloned().unwrap_or_else(|| json!("Assertion failed")),
                );
                return TaskResult(r);
            }
        }
    }
    r.insert(
        "msg".into(),
        item.args
            .get("success_msg")
            .cloned()
            .unwrap_or_else(|| json!("All assertions passed")),
    );
    TaskResult(r)
}

/// How long a `pause` waits and in which unit it was asked, `None` for a prompt with no
/// duration, or the failure its arguments get.
fn pause_duration(module: &str, item: &Item) -> Result<Option<(i64, &'static str)>, TaskResult> {
    if let Some(refusal) = unsupported(module, &PAUSE, &[], &item.args) {
        return Err(unchanged_failure(refusal));
    }
    let whole = |v: &Value| match v {
        Value::Number(n) => n.as_f64().map(|f| f.trunc() as i64),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    let asked = match (item.args.get("minutes"), item.args.get("seconds")) {
        (Some(_), Some(_)) => {
            return Err(TaskResult::failed_with(
                "parameters are mutually exclusive: minutes|seconds",
            ));
        }
        (Some(v), None) => whole(v).map(|m| (m.saturating_mul(60), "minutes")).ok_or(v),
        (None, Some(v)) => whole(v).map(|s| (s, "seconds")).ok_or(v),
        (None, None) => Err(&Value::Null),
    };
    let asked = match asked {
        Ok(asked) => Some(asked),
        Err(Value::Null) => None,
        Err(v) => {
            return Err(TaskResult::failed_with(format!(
                "non-integer value given for prompt duration: {v}"
            )));
        }
    };
    // Python's clock counts nanoseconds in a signed 64-bit integer, and the reference fails a
    // pause it cannot hold rather than wait it out. Measured on ansible-core 2.19.12.
    if asked.is_some_and(|(seconds, _)| seconds > i64::MAX / 1_000_000_000) {
        return Err(TaskResult::failed_with(PYTIME_OUT_OF_RANGE));
    }
    Ok(asked)
}

/// What a `pause` with a duration prints on stdout before it waits, measured on ansible-core
/// 2.19.12: `Pausing for N seconds`, at least one, `(output is hidden)` behind it under `echo:
/// false`, and the interrupt keys, ending in `\r`, when it also has a `prompt`.
pub(in crate::executor) fn pause_lines(module: &str, item: &Item) -> Vec<String> {
    let Ok(Some((seconds, _))) = pause_duration(module, item) else {
        return Vec::new();
    };
    let echo = item
        .args
        .get("echo")
        .and_then(volant_protocol::modules::arg_bool)
        .unwrap_or(true);
    let hidden = if echo { "" } else { " (output is hidden)" };
    let mut lines = vec![format!("Pausing for {} seconds{hidden}", seconds.max(1))];
    if item.args.get("prompt").is_some_and(|p| !python_falsy(p)) {
        lines.push("(ctrl+C then 'C' = continue early, ctrl+C then 'A' = abort)\r".to_string());
    }
    lines
}

/// `pause`, measured on ansible-core 2.19.12: a duration under one second waits one, `delta` is
/// the whole seconds waited, and `stdout` gives the time waited to two decimals, in minutes
/// unless `seconds` was the argument. A pause that asks for an answer, through `prompt` or by
/// having no duration at all, warns and goes on at once when standard input is not a terminal.
///
/// On a terminal a pause with no duration asks, as the reference does (`[name]`, the prompt,
/// the answer in `user_input`). One that has a duration and a `prompt` is refused: nothing here
/// waits for the answer and the time at once. `interactive` is passed in so that both sides of
/// that can be tested. `None` is the run's interruption, which ends the wait early.
pub(super) async fn pause(
    module: &str,
    name: &str,
    item: &Item,
    interactive: bool,
    stop: &mut watch::Receiver<bool>,
) -> Option<TaskResult> {
    let asked = match pause_duration(module, item) {
        Ok(asked) => asked,
        Err(refused) => return Some(refused),
    };
    if interactive && asked.is_some() && item.args.contains_key("prompt") {
        return Some(TaskResult::failed_with(PROMPT_REFUSED));
    }
    let echo = item
        .args
        .get("echo")
        .and_then(volant_protocol::modules::arg_bool)
        .unwrap_or(true);
    let mut r = Map::new();
    r.insert("start".into(), json!(timestamp()));
    let began = std::time::Instant::now();
    let mut user_input = String::new();
    match asked {
        Some((seconds, _)) => {
            let wait = Duration::from_secs(seconds.max(1).unsigned_abs());
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = interrupted(stop) => return None,
            }
        }
        None if interactive => {
            let prompt = match item.args.get("prompt") {
                Some(p) if !python_falsy(p) => {
                    p.as_str().map_or_else(|| p.to_string(), str::to_string)
                }
                _ => "Press enter to continue, Ctrl+C to interrupt".to_string(),
            };
            let hidden = if echo { "" } else { " (output is hidden)" };
            let message = format!(
                "[{}]
{prompt}{hidden}:",
                name.trim()
            );
            let read =
                tokio::task::spawn_blocking(move || crate::prompt::at_terminal(&message, !echo));
            let answer = tokio::select! {
                answer = read => answer,
                () = interrupted(stop) => return None,
            };
            match answer {
                Ok(Ok(line)) => user_input = line,
                Ok(Err(e)) => return Some(TaskResult::failed_with(format!("{e:#}"))),
                Err(e) => return Some(TaskResult::failed_with(format!("reading the answer: {e}"))),
            }
        }
        None => {
            r.insert(
                "warnings".into(),
                json!(["Not waiting for response to prompt as stdin is not interactive"]),
            );
        }
    }
    let waited = began.elapsed();
    let unit = asked.map_or("minutes", |(_, unit)| unit);
    let shown = waited.as_secs_f64() / if unit == "minutes" { 60.0 } else { 1.0 };
    let shown = (shown * 100.0).round() / 100.0;
    // Python prints a whole float with one decimal, `1.0`, and any other the shortest way.
    let shown = if shown.fract() == 0.0 {
        format!("{shown:.1}")
    } else {
        shown.to_string()
    };
    r.insert("changed".into(), json!(false));
    r.insert("delta".into(), json!(waited.as_secs()));
    r.insert("echo".into(), json!(echo));
    r.insert("rc".into(), json!(0));
    r.insert("stderr".into(), json!(""));
    r.insert("stdout".into(), json!(format!("Paused for {shown} {unit}")));
    r.insert("stop".into(), json!(timestamp()));
    r.insert("user_input".into(), json!(user_input));
    Some(TaskResult(r))
}

/// Resolves when the run is interrupted, and never once nothing is left that could interrupt it.
async fn interrupted(stop: &mut watch::Receiver<bool>) {
    let gone = stop.wait_for(|stopped| *stopped).await.is_err();
    if gone {
        std::future::pending::<()>().await;
    }
}

/// Now, in the shape the reference's `pause` gives `start` and `stop`. UTC where the reference
/// uses local time, as the agent's own timestamps are.
fn timestamp() -> String {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since.as_secs() as i64;
    // Days since 1970-01-01 to a Gregorian date, Howard Hinnant's algorithm.
    let z = secs.div_euclid(86_400) + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let day = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:06}",
        day / 3600,
        (day % 3600) / 60,
        day % 60,
        since.subsec_micros()
    )
}
