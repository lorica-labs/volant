// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading and splicing a compiled play: notified handlers, batches and the fixed names.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::inventory::Host;
use crate::playbook::PlayTask;
use crate::stats::Refusal;
use crate::template::Templar;

use super::model::{Compiled, Step, StepKind};
impl Compiled {
    /// Inserts `steps` at `at`, growing every block range that ends at or after `at` and shifting
    /// every range that starts at or after it.
    ///
    /// Called by the coordinator alone, at a flush point, once every host has reported that step
    /// and before any of them reads the list again - which is what keeps the index each driver
    /// holds pointing at the step it was pointing at. A range that *ends* at `at` has to grow:
    /// `at` is one past the flush, so a flush written last in a block's body ends that body
    /// exactly there, and testing `>` instead of `>=` would drop the spliced handlers into that
    /// block's rescue, where a host that failed nothing never goes.
    pub(crate) fn splice(&mut self, at: usize, steps: Vec<Step>) {
        let n = steps.len();
        for span in &mut self.blocks {
            for range in [&mut span.body, &mut span.rescue, &mut span.always] {
                if range.start >= at {
                    range.start += n;
                }
                if range.end >= at {
                    range.end += n;
                }
            }
        }
        // Marked as boundaries until the next `mark_boundaries` says otherwise, so a splice
        // nobody marks is the strict `linear` and never a host running ahead.
        if self.crosses.len() == self.steps.len() {
            self.crosses.splice(at..at, std::iter::repeat_n(true, n));
        }
        self.steps.splice(at..at, steps);
    }
}

/// The file a task statement's relative name points at: the one beside the file that wrote the
/// statement, and failing that the one in the role's own `tasks/`.
///
/// Measured on ansible-core 2.19.12 with a role whose `tasks/sub/deep.yml` names `shared.yml`, in
/// all three states - only `tasks/shared.yml`, only `tasks/sub/shared.yml`, both - and the one
/// beside the writer wins whenever it is there. The fallback is what a role like UBUNTU22-CIS
/// needs: `tasks/section_1/cis_1.1.1.x.yml` writes `file: warning_facts.yml` and means
/// `tasks/warning_facts.yml`.
///
/// One function for `import_tasks` and `include_tasks` both: the static and the dynamic statement
/// read the same name the same way, and two copies of this rule would be two chances to move only
/// one of them. The path it returns is the one to report as missing when nothing is there, which
/// is why it answers with a path rather than an `Option`.
pub(crate) fn beside_or_in_role(file_dir: &Path, role_dir: Option<&Path>, name: &str) -> PathBuf {
    let beside = file_dir.join(name);
    match role_dir.map(|role| role.join("tasks").join(name)) {
        Some(in_role) if !beside.is_file() && in_role.is_file() => in_role,
        _ => beside,
    }
}

/// The handlers one name notifies, in definition order: the **first** one carrying it as its
/// `name`, and **every** one listening for it.
///
/// The asymmetry is measured on ansible-core 2.19.12, both halves on their own: two handlers
/// written with the same name run the first alone, and a handler named `x` beside one listening
/// for `x` both run when `x` is notified. It is what makes a role that runs three times
/// contribute one handler rather than three - the three copies share a name, and only the first
/// of them is ever reached.
///
/// `names` is what each handler's `name` renders to for the host asking, one per handler in
/// order, `None` for one that does not render: a handler name may be a template, `listen` never is.
pub(crate) fn resolve_notify(c: &Compiled, name: &str, names: &[Option<String>]) -> Vec<usize> {
    let named = names.iter().position(|n| n.as_deref() == Some(name));
    c.handlers
        .iter()
        .enumerate()
        .filter(|(i, h)| named == Some(*i) || h.listen.iter().any(|l| l == name))
        .map(|(i, _)| i)
        .collect()
}

/// The steps a flush point at `at` inserts behind itself: one per handler, in definition order.
///
/// All of them, not only the notified ones, because the coordinator decides this for every host
/// at once and which handlers a host notified is the driver's own business. A step for a handler
/// the host did not notify reports nothing and shows nothing.
///
/// They carry the flush step's own block and section, which is what makes the spliced steps part
/// of the section the flush was written in: measured on ansible-core 2.19.12, a handler that
/// fails at a flush written inside a block is taken by that block's `rescue` (`rescued=1`, exit
/// 0), and a host that never entered a rescue holding the flush steps over the handlers too.
///
/// And they carry its mask, for the same reason. A `meta: flush_handlers` inside an
/// `include_tasks` only one host asked for is spliced in for that host alone; handler steps left
/// unmasked behind it would be walked by every host of the batch, and each of them would run the
/// handlers **it** had notified, at a flush it was never asked to perform.
pub(crate) fn handler_steps(c: &Compiled, at: usize) -> Vec<Step> {
    let (block, section) = (c.steps[at].block, c.steps[at].section);
    c.handlers
        .iter()
        .enumerate()
        .map(|(i, h)| Step {
            kind: StepKind::Handler(i),
            task: h.task.clone(),
            block,
            section,
            role: h.role,
            origin: Arc::clone(&c.steps[at].origin),
            include_params: c.steps[at].include_params.clone(),
            hosts: c.steps[at].hosts.clone(),
        })
        .collect()
}

/// Every `meta` action ansible-core 2.19.12 accepts. `flush_handlers` compiles into a flush point of
/// its own; the executor runs the rest where the step stands.
pub(crate) const META_ACTIONS: &[&str] = &[
    "clear_facts",
    "clear_host_errors",
    "end_batch",
    "end_host",
    "end_play",
    "end_role",
    "flush_handlers",
    "noop",
    "refresh_inventory",
    "reset_connection",
];

/// The `meta` action that runs the notified handlers where it stands.
pub(crate) const FLUSH_HANDLERS: &str = "flush_handlers";

/// The action a `meta` task asked for.
pub(crate) fn meta_action(task: &PlayTask) -> &str {
    task.args
        .get("_raw_params")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The play's hosts cut into the batches `serial` asks for, in inventory order.
///
/// This is `PlaybookExecutor._get_serialized_batches` as ansible-core 2.19.12 writes it, and
/// three of its properties were measured rather than derived. A percentage is taken of the
/// **whole** host list and never of what is left, so `[1, "50%"]` on three hosts gives three
/// batches of one rather than one and then two. A size that comes out at or below zero means
/// every remaining host in one batch and ends the cutting there, which is how `serial: 0`,
/// `serial: -1`, `serial: []` and a `serial` larger than the inventory all end up as the single
/// batch this engine ran before this keyword existed. And a percentage that truncates to zero is
/// one host, not none: `"10%"` of three hosts is one, so a small percentage cannot silently ask
/// for a batch of nobody.
///
/// The hosts handed in are the play's **resolved** hosts, failures of earlier plays included.
/// Measured: after `h1` fails, a second play over `all` with `serial: 2` still cuts `[h1, h2]`
/// and `[h3]`, and prints a banner for each - filtering the failed host out first would merge
/// the two batches into one and run `h2` and `h3` together, which is not what the reference does.
pub(crate) fn batches(
    hosts: &[Host],
    serial: Option<&Value>,
    templar: &Templar,
    vars: &Map<String, Value>,
) -> anyhow::Result<Vec<Vec<Host>>> {
    // The reference stores `serial` as a list, so a scalar is a one-element list and an empty
    // list is `[-1]`, which is "every host at once". An absent `serial` reaches the same place.
    let rendered = match serial {
        None => Value::Null,
        // Measured, exit 4: `serial: "{{ n }}"` with no `n` says `Error processing keyword
        // 'serial': 'n' is undefined` and stops the run there, before the play's banner.
        Some(raw) => templar.render_value(raw, vars).map_err(|err| {
            Refusal::at(4, format!("Error processing keyword 'serial': {}", err.0))
        })?,
    };
    let sizes: Vec<Value> = match rendered {
        Value::Null => Vec::new(),
        Value::Array(values) => values,
        other => vec![other],
    };
    let total = hosts.len();
    let mut out: Vec<Vec<Host>> = Vec::new();
    let mut rest = hosts;
    let mut item = 0;
    while !rest.is_empty() {
        // Past the end of the list the last element repeats, which is what makes `serial: [1]`
        // and `serial: 1` the same request.
        let size = match sizes.get(item.min(sizes.len().saturating_sub(1))) {
            None => -1,
            Some(value) => serial_size(value, total)?,
        };
        if size <= 0 {
            out.push(rest.to_vec());
            break;
        }
        let size = (size as usize).min(rest.len());
        out.push(rest[..size].to_vec());
        rest = &rest[size..];
        item += 1;
    }
    Ok(out)
}

/// One `serial` element as a number of hosts, following `ansible.utils.helpers.pct_to_int`:
/// `int(value)` for anything but a percentage, and `int(pct / 100 * total) or 1` for one.
///
/// `int()` truncates towards zero rather than rounding, and its `or` turns a zero into one; a
/// negative stays negative and the caller reads it as "all of them". Returned signed for that
/// reason: clamping to zero here would lose the difference between `serial: 0`, which is every
/// host, and a batch of none, which would spin this loop for ever.
fn serial_size(value: &Value, total: usize) -> anyhow::Result<i64> {
    // The reference lets Python's own `ValueError` out here: it prints `Unexpected Exception,
    // this is probably a bug: invalid literal for int() with base 10: 'abc'` with a traceback and
    // exits 250. Measured, and a deliberate divergence: the sentence is kept, the crash is not,
    // so a `serial` nobody can read is refused at exit 4 like the rest of an unusable play.
    let refused = |value: &Value| -> anyhow::Error {
        let shown = value
            .as_str()
            .map_or_else(|| crate::render::ansible_json(value), str::to_string);
        Refusal::at(
            4,
            format!(
                "Error processing keyword 'serial': invalid literal for int() with base 10: \
                 '{shown}'"
            ),
        )
    };
    match value {
        // `int(2.7)` is 2 there, and `int(True)` is 1.
        Value::Number(n) => Ok(n.as_f64().unwrap_or_default().trunc() as i64),
        Value::Bool(b) => Ok(i64::from(*b)),
        // The reference tests `value.endswith('%')` on the **raw** string, trailing whitespace
        // included: `"50% "` (a space after the sign) fails that test there, falls to `int()`,
        // and crashes the same way `serial: abc` does. `trim_end()` here used to accept it as a
        // percentage instead of refusing it - measured divergence, fixed by matching the raw
        // string like the reference does.
        Value::String(s) if s.ends_with('%') => {
            let pct: i64 = s
                .trim()
                .trim_end_matches('%')
                .trim()
                .parse()
                .map_err(|_| refused(value))?;
            let size = (pct as f64 / 100.0 * total as f64).trunc() as i64;
            Ok(if size == 0 { 1 } else { size })
        }
        Value::String(s) => s.trim().parse::<i64>().map_err(|_| refused(value)),
        other => Err(refused(other)),
    }
}
