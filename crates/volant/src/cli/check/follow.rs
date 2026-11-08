// SPDX-License-Identifier: GPL-3.0-or-later
//! The dynamic includes a check can follow without a host: a plain file name or role name, read
//! and checked as the run checks it when a host reaches the statement.

use std::collections::HashSet;

use serde_json::Value;

use super::report::{Kind, Reason};
use crate::compile::{
    self, Compiled, IncludeKind, IncludeParams, IncludeRequest, IncludeTarget, Step, StepKind,
};
use crate::template::Templar;

/// Every expansion reachable from `compiled` through includes whose name is plain text, each file
/// or role once. A templated name is an `unknown` reason in `reasons`; a refusal inside what an
/// include brings in, or an include naming nothing that exists, is the `Err`.
pub(super) fn includes<'a>(
    compiled: impl Iterator<Item = &'a Compiled>,
    reasons: &mut Vec<Reason>,
) -> Result<Vec<Compiled>, Reason> {
    let mut seen = HashSet::new();
    let mut expanded = Vec::new();
    for base in compiled {
        expanded.extend(expand_steps(base, &mut seen, reasons)?);
    }
    let mut i = 0;
    while i < expanded.len() {
        let more = expand_steps(&expanded[i], &mut seen, reasons)?;
        expanded.extend(more);
        i += 1;
    }
    Ok(expanded)
}

fn expand_steps(
    base: &Compiled,
    seen: &mut HashSet<String>,
    reasons: &mut Vec<Reason>,
) -> Result<Vec<Compiled>, Reason> {
    let mut out = Vec::new();
    for step in &base.steps {
        let StepKind::Include(kind) = step.kind else {
            continue;
        };
        // A ring of files including each other is stopped by the run at this depth too.
        if step.origin.depth as usize + 1 > compile::DEPTH {
            continue;
        }
        let Some(target) = target(base, step, kind, seen, reasons)? else {
            continue;
        };
        let request = IncludeRequest {
            target,
            what: String::new(),
            vars: IncludeParams::default(),
            label: None,
            role_id: None,
        };
        let expansion = compile::expand_include(base, step, &request).map_err(|err| {
            // A refusal inside the file names the task that carries it, as a run's would.
            let tasks = match &request.target {
                IncludeTarget::File(path) => crate::playbook::parse_tasks_file(path).ok(),
                IncludeTarget::Role(_) => None,
            };
            let inside = tasks
                .as_deref()
                .and_then(|items| super::first_refused(super::listed(items).into_iter()));
            super::reason_of(&err, inside.or(step.task.origin.as_ref()))
        })?;
        out.push(expansion);
    }
    Ok(out)
}

/// What the statement names, `None` when it names it by a template (said in `reasons`) or was
/// followed already.
fn target(
    base: &Compiled,
    step: &Step,
    kind: IncludeKind,
    seen: &mut HashSet<String>,
    reasons: &mut Vec<Reason>,
) -> Result<Option<IncludeTarget>, Reason> {
    let task = &step.task;
    let at = task.origin.as_ref();
    match kind {
        IncludeKind::Tasks => {
            let Some(name) = task
                .args
                .get("file")
                .or_else(|| task.args.get("_raw_params"))
                .and_then(Value::as_str)
            else {
                return Ok(None);
            };
            if Templar::is_template(name) {
                reasons.push(super::unknown(
                    format!(
                        "{} with a templated file name: checked when the run reaches it",
                        task.module
                    ),
                    at,
                ));
                return Ok(None);
            }
            let path = compile::beside_or_in_role(
                &step.origin.file_dir,
                step.origin.role_dir.as_deref(),
                name,
            );
            if !seen.insert(format!("file {}", path.display())) {
                return Ok(None);
            }
            if !path.is_file() {
                return Err(super::refusal(
                    Kind::Invalid,
                    format!(
                        "{} names '{name}', and there is no file {}",
                        task.module,
                        path.display()
                    ),
                    at,
                ));
            }
            Ok(Some(IncludeTarget::File(path)))
        }
        IncludeKind::Role => {
            let entry = compile::include_role_entry(&task.args)
                .map_err(|err| super::reason_of(&err, at))?;
            if Templar::is_template(&entry.name) {
                reasons.push(super::unknown(
                    format!(
                        "{} with a templated role name: checked when the run reaches it",
                        task.module
                    ),
                    at,
                ));
                return Ok(None);
            }
            if !seen.insert(format!("role {}", entry.name)) {
                return Ok(None);
            }
            base.search
                .locate(&entry.name)
                .map_err(|err| super::refusal(Kind::Invalid, format!("{err:#}"), at))?;
            Ok(Some(IncludeTarget::Role(Box::new(entry))))
        }
    }
}
