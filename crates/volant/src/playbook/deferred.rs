// SPDX-License-Identifier: GPL-3.0-or-later
//! Keywords written as templates, which the reference renders for each host when the task runs.

use anyhow::bail;
use saphyr::{Scalar, Yaml};

use crate::template::Templar;
use crate::yaml::field;

use super::keywords::boolean;

/// The keywords of a play, a block, a role entry or a task whose value is a template.
///
/// A keyword written as a literal is read once, at load, into the typed field that carries it. A
/// keyword written as a template cannot be read then: its value depends on the host and on what
/// ran before it. Its text waits here and the typed field stays unset, until the executor renders
/// it for the host that reaches the task and puts the result back into a copy of the task. The
/// layer closest to the task wins over the layers outside it, whichever form it is written in.
///
/// Measured on ansible-core 2.19.12, all of them are rendered when the task runs, and a value
/// that is not what the keyword needs fails the task with `Error processing keyword`, where a
/// literal of the same kind is refused at load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deferred {
    pub r#become: Option<String>,
    pub no_log: Option<String>,
    pub run_once: Option<String>,
    pub any_errors_fatal: Option<String>,
    pub ignore_unreachable: Option<String>,
    pub delegate_facts: Option<String>,
    pub timeout: Option<String>,
    /// `async`, which the reference names `async_val` in its own messages.
    pub async_val: Option<String>,
    pub poll: Option<String>,
    /// The two play-only keywords the engine renders when the play starts, with the play's own
    /// variables, like `serial`.
    pub gather_facts: Option<String>,
    pub force_handlers: Option<String>,
    /// A module an `action:` statement named by a template that needs variables, rendered for
    /// each host when the task runs, with the arguments the statement gave it as written: a
    /// `key=value` string or a mapping.
    pub module: Option<(String, serde_json::Value)>,
}

impl Deferred {
    /// Whether any keyword of this layer waits to be rendered.
    pub fn any(&self) -> bool {
        self != &Self::default()
    }
}

/// A template written for a keyword: the text of a string that holds Jinja syntax.
fn template<'a>(yaml: &'a Yaml, key: &str) -> Option<&'a str> {
    match field(yaml, key) {
        Some(Yaml::Value(Scalar::String(s))) if Templar::is_template(s) => Some(s),
        _ => None,
    }
}

/// A boolean keyword that may be a template. A template goes to `slot` and the keyword reads as
/// unset; anything else is read, and refused when it is not a boolean, as [`boolean`] does.
pub(super) fn boolean_or_template(
    yaml: &Yaml,
    key: &str,
    slot: &mut Option<String>,
) -> anyhow::Result<Option<bool>> {
    match template(yaml, key) {
        Some(text) => {
            *slot = Some(text.to_string());
            Ok(None)
        }
        None => boolean(yaml, key),
    }
}

/// A keyword counting seconds that may be a template: `timeout`, `async` and `poll`.
///
/// A negative `timeout` goes to `slot` as well: measured on ansible-core 2.19.12, `timeout:
/// -1` fails its task when it runs, with `Timeout -1 is invalid, it must be between 0 and
/// 100000000.`, which `ignore_errors` and `rescue` take, and so does one that is no number.
pub(super) fn seconds_or_template(
    yaml: &Yaml,
    key: &str,
    context: &str,
    slot: &mut Option<String>,
) -> anyhow::Result<Option<u64>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(Yaml::Value(Scalar::Integer(i))) if *i >= 0 => Ok(Some(*i as u64)),
        Some(Yaml::Value(Scalar::Integer(i))) if key == "timeout" => {
            *slot = Some(i.to_string());
            Ok(None)
        }
        Some(Yaml::Value(Scalar::Integer(i))) => {
            bail!("{context}'{key}': the value {i} is negative, which is not supported yet")
        }
        Some(Yaml::Value(Scalar::String(s))) if Templar::is_template(s) || key == "timeout" => {
            *slot = Some(s.to_string());
            Ok(None)
        }
        Some(other) => {
            bail!("{context}'{key}' must be a non-negative integer, found {other:?}")
        }
    }
}
