// SPDX-License-Identifier: GPL-3.0-or-later
//! Which steps of a play the hosts meet in front of, read off the task text and the variables.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{Map, Value};
use volant_protocol::modules::{is_builtin, short_name};

use crate::compile::{Compiled, Step, after, after_failure, rescue_target};
use crate::playbook::PlayTask;
use crate::template::Templar;
use crate::vars::VarStore;

use crate::executor::prepare::PlayPlan;
/// Names whose value depends on what the other hosts have done: another host's variables, and
/// the play's own live host list. A task whose raw text mentions one of them must not run ahead
/// of the others, so `linear` puts a boundary in front of it.
const CROSS_HOST_NAMES: [&str; 3] = ["hostvars", "play_hosts", "play_batch"];

/// Words that, inside an expression, read a variable whose name is only known once it renders -
/// `vars[...]`, `vars.get(...)`, `lookup('vars', ...)` - or read a template file this scan never
/// opens, `lookup('template', ...)`. Either can land on another host's value.
const DYNAMIC_READS: [&str; 2] = ["vars", "template"];

/// The template statements that pull another file's text in, which the body scan does not
/// follow.
const TEMPLATE_PULLS: [&str; 4] = ["include", "import", "from", "extends"];

/// Whether the hosts of a play meet in front of step `pos`. With `strict`, every step is such a
/// point: that is what `linear` means, and a task's text cannot show a dependency that runs
/// through a file, a database or a service. Otherwise the keyword table and the step's mark
/// decide (see [`mark_boundaries`]), and a host carries on through the steps in between.
///
/// The step loop asks it twice on the way through an iteration, once to give back a fork permit
/// kept from the batch before and once to stop and wait for the other hosts. What a batch end
/// asks instead is whether this driver carries straight on, which is a different question.
pub(super) fn is_boundary(c: &Compiled, pos: usize, strict: bool) -> bool {
    if strict || c.steps[pos].task.barrier() {
        return true;
    }
    // A list nobody marked is a list where every step is a boundary.
    c.crosses.len() != c.steps.len() || c.crosses[pos]
}

/// Whether the hosts of the play meet in front of step `pos`. Under `free` and `host_pinned` they
/// meet only where a step is one by its keywords - a `run_once`, a `pause`, an
/// `any_errors_fatal` - and not where its text reads another host or batching is off.
pub(super) fn meets(plan: &PlayPlan, c: &Compiled, pos: usize, strict: bool) -> bool {
    if plan.strategy.free() {
        c.steps[pos].task.barrier()
    } else {
        is_boundary(c, pos, strict)
    }
}

/// Whether the hosts meet in front of every step: `[volant] batching` is off, or a fact the
/// playbook wrote while the run went is still a template (see [`VarStore::templated_facts`]).
/// Asked again at every step, because the second half can turn on at any of them.
pub(super) fn strict(batching: bool, store: &Mutex<VarStore>) -> bool {
    !batching || store.lock().expect("vars lock").templated_facts()
}

/// The builtin modules of ansible-core 2.19.12 that answer with `ansible_facts`: every file of
/// `ansible/modules/` and `ansible/plugins/action/` that sets the key, read off the source with
/// `grep -l ansible_facts` and checked one by one (the `dnf` action writes `pkg_mgr`; `package`,
/// `service`, `reboot` and `wait_for_connection` only read facts). `set_fact` and `include_vars`
/// run on the controller and never join a batch; they are here because they write facts all the
/// same.
pub(super) const FACT_MODULES: [&str; 10] = [
    "dnf",
    "gather_facts",
    "getent",
    "hostname",
    "include_vars",
    "mount_facts",
    "package_facts",
    "service_facts",
    "set_fact",
    "setup",
];

/// Whether a remote task's module can write this host's facts, so that a batch ends behind it:
/// every task after it in the same batch is rendered before it runs, and would read the facts
/// as they stood before. A builtin answers from [`FACT_MODULES`]; any other module - a
/// collection's, a role's `library/` - may return `ansible_facts`, so it ends its batch too.
pub(super) fn writes_facts(task: &PlayTask) -> bool {
    !is_builtin(&task.module) || FACT_MODULES.contains(&short_name(&task.module))
}

/// Whether rendering a task calls a lookup: `lookup`, `query` or `q` in an expression, or a
/// `with_<lookup>` loop. A lookup reads the controller, a file or a command's output, at the
/// moment it renders.
pub(super) fn calls_lookup(task: &PlayTask) -> bool {
    let (text, bare) = task_strings(task);
    let mut read = BTreeSet::new();
    for s in &text {
        for expr in expressions(s) {
            words(expr, &mut read);
        }
    }
    for s in &bare {
        words(s, &mut read);
    }
    task.loop_with.is_some() || ["lookup", "query", "q"].iter().any(|w| read.contains(*w))
}

/// Whether a task reads a file on the controller: a module that sends one (`copy`, `template`,
/// `unarchive`, `assemble`, `script`), or a lookup.
fn reads_controller_files(task: &PlayTask) -> bool {
    matches!(
        short_name(&task.module),
        "copy" | "template" | "unarchive" | "assemble" | "script"
    ) || calls_lookup(task)
}

/// Whether a task can write a file on the controller: `fetch`, or a task run somewhere other
/// than its own host - `delegate_to` (which may name the controller), or task variables setting
/// `ansible_connection: local`.
fn writes_controller_files(task: &PlayTask) -> bool {
    short_name(&task.module) == "fetch"
        || task.delegate_to.is_some()
        || crate::vars::host_setting(&task.vars, "ansible_connection")
            .is_some_and(|c| c.as_str() != Some("ssh"))
}

/// Whether a text names one of [`CROSS_HOST_NAMES`].
fn mentions(s: &str) -> bool {
    // `ansible_play_hosts_all` is a static copy of the play's starting host list: no host can
    // ever change it, so matching it buys no ordering guarantee and only costs a barrier. Strip
    // it before matching so it does not trip the `play_hosts` needle on its own; a task that
    // separately mentions `ansible_play_hosts` or `play_batch` still is a boundary.
    let s = s.replace("play_hosts_all", "");
    CROSS_HOST_NAMES.iter().any(|n| s.contains(n))
}

/// Every string in `value`, keys included, at any depth.
pub(crate) fn strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(s) => out.push(s),
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(map) => {
            for (k, v) in map {
                out.push(k);
                strings(v, out);
            }
        }
        _ => {}
    }
}

/// What sits between `{{ }}` and between `{% %}` in a text. An opening nothing closes runs to the
/// end, which reads more rather than less.
pub(crate) fn expressions(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('{') {
        let after = &rest[at + 1..];
        let close = match after.as_bytes().first() {
            Some(b'{') => "}}",
            Some(b'%') => "%}",
            _ => {
                rest = after;
                continue;
            }
        };
        let body = &after[1..];
        let end = body.find(close).unwrap_or(body.len());
        out.push(&body[..end]);
        rest = &body[(end + close.len()).min(body.len())..];
    }
    out
}

/// Every word of `text` that could name a variable, and a good many that do not: a filter, a
/// test, a word inside a string. Reading too many only costs a barrier.
fn words(text: &str, out: &mut BTreeSet<String>) {
    for word in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            out.insert(word.to_string());
        }
    }
}

/// Every string of a task that is rendered before it runs: the templated text, and apart from it
/// the bare expressions (`when` and its kin, `debug`'s `var`, `assert`'s `that`), which render
/// without braces.
pub(crate) fn task_strings(task: &PlayTask) -> (Vec<&str>, Vec<&str>) {
    let mut text: Vec<&str> = vec![&task.name];
    let mut bare: Vec<&str> = task
        .when
        .iter()
        .chain(&task.changed_when)
        .chain(&task.failed_when)
        .chain(&task.until)
        .map(String::as_str)
        .collect();
    // A templated `ignore_errors` renders per item like a condition.
    if let Some(crate::playbook::Flag::Template(text)) = &task.ignore_errors {
        bare.push(text);
    }
    let module = short_name(&task.module);
    for (key, value) in &task.args {
        if (module == "debug" && key == "var") || (module == "assert" && key == "that") {
            strings(value, &mut bare);
        } else {
            text.push(key);
            strings(value, &mut text);
        }
    }
    for (key, value) in &task.vars {
        text.push(key);
        strings(value, &mut text);
    }
    for value in task
        .loop_items
        .iter()
        .chain(&task.retries)
        .chain(&task.delay)
        .chain(&task.environment)
        .chain(&task.module_defaults)
    {
        strings(value, &mut text);
    }
    text.extend(
        [&task.delegate_to, &task.become_user, &task.loop_label]
            .into_iter()
            .flatten()
            .map(String::as_str),
    );
    (text, bare)
}

/// Whether a task names another host's state in its own text: [`CROSS_HOST_NAMES`] anywhere in
/// what renders before it runs. Decided from the unrendered text, never from what it renders to.
pub(super) fn reads_across_hosts(task: &PlayTask) -> bool {
    let (text, bare) = task_strings(task);
    text.iter().chain(&bare).any(|s| mentions(s))
}

/// A play's static variable definitions, as far as the barrier cares: for each name, whether its
/// value reads another host on its own, and every word its expressions name.
///
/// Names are pooled over every host, role and layer: a name that reads another host anywhere is
/// treated as reading one everywhere. That is wider than any one host's view and never narrower.
#[derive(Debug, Clone, Default)]
pub(in crate::executor) struct Definitions(Vec<(String, bool, BTreeSet<String>)>);

impl Definitions {
    pub(in crate::executor) fn add(&mut self, map: &Map<String, Value>) {
        for (name, value) in map {
            let mut texts = Vec::new();
            strings(value, &mut texts);
            let mut named = BTreeSet::new();
            let mut direct = false;
            for text in texts {
                direct |= mentions(text);
                for expr in expressions(text) {
                    words(expr, &mut named);
                }
            }
            direct |= DYNAMIC_READS.iter().any(|w| named.contains(*w));
            self.0.push((name.clone(), direct, named));
        }
    }

    /// The names whose value can read another host's: directly, or through another such name,
    /// however many names deep.
    fn crossing(&self) -> BTreeSet<String> {
        let mut crossing: BTreeSet<String> = self
            .0
            .iter()
            .filter(|(_, direct, _)| *direct)
            .map(|(name, _, _)| name.clone())
            .collect();
        loop {
            let before = crossing.len();
            for (name, _, named) in &self.0 {
                if !crossing.contains(name) && !named.is_disjoint(&crossing) {
                    crossing.insert(name.clone());
                }
            }
            if crossing.len() == before {
                return crossing;
            }
        }
    }
}

/// Marks the steps of `c` that read what another host did, so that `is_boundary` stops the
/// hosts in front of them under `[volant] batching`. Called once the list is laid out and again
/// behind every splice: no host is past a splice point while it waits for one, so a mark that
/// changes behind it changes nothing a host has already walked.
///
/// A step reads another host when its own text names one ([`reads_across_hosts`]); when an
/// expression of it reads a variable by a name only known at render time ([`DYNAMIC_READS`]);
/// when it names a variable whose static definition reads another host, however deep
/// ([`Definitions::crossing`] over `base`, the play's roles and their parameters); when it is a
/// `template` whose file does any of that, or whose file cannot be read here; when it runs on
/// another host (`delegate_to`); when it reads a controller file behind a task that can write
/// one; from the first task that writes its facts onto the host it delegated to on; and when it
/// is the step a host reaches next after any of those.
///
/// The last rule is the other half of the fence. A boundary makes the reader wait until every
/// host is past the step before it; nothing yet stops the host being read from running on past
/// the reader and changing what the reader is about to read. The strict `linear` has a barrier
/// behind the reader as well, and so does this: every step a host can go to from a marked one
/// (the next step, the one past a rescue, the rescue and the `always` a failure leads to) waits
/// until every host is done with the marked one.
pub(in crate::executor) fn mark_boundaries(
    c: &mut Compiled,
    base: &Definitions,
    playbook_dir: &Path,
) {
    let mut defs = base.clone();
    for role in c.roles.iter().chain([&c.exported]) {
        defs.add(&role.defaults);
        defs.add(&role.vars);
        defs.add(&role.params);
    }
    let crossing = defs.crossing();
    // A delegated task with `delegate_facts` writes into another host's variables, and that host
    // reads them as its own, through a name no scan can tie back to the writer. The writer and
    // every step behind it are boundaries, so no host can write before the others have read what
    // came before, nor run past the write.
    let writes_elsewhere = c
        .steps
        .iter()
        .position(|s| s.task.delegates_facts() && s.task.delegate_to.is_some());
    // A file one host fetched, or wrote on the controller, is read by the others from the
    // controller: every task behind the first writer that reads a controller file waits for it.
    let controller_written = c
        .steps
        .iter()
        .position(|s| writes_controller_files(&s.task));
    let marked: Vec<bool> = c
        .steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            writes_elsewhere.is_some_and(|w| i >= w)
                || step.task.barrier()
                || step.task.delegate_to.is_some()
                || (controller_written.is_some_and(|w| i > w) && reads_controller_files(&step.task))
                || step_crosses(step, &crossing, playbook_dir)
        })
        .collect();
    let mut crosses = marked.clone();
    for (k, _) in marked.iter().enumerate().filter(|(_, m)| **m) {
        let failure = after_failure(c, k).map(|(next, _)| next);
        for next in [Some(k + 1), Some(after(c, k)), rescue_target(c, k), failure]
            .into_iter()
            .flatten()
        {
            if let Some(slot) = crosses.get_mut(next) {
                *slot = true;
            }
        }
    }
    c.crosses = crosses;
}

fn step_crosses(step: &Step, crossing: &BTreeSet<String>, playbook_dir: &Path) -> bool {
    let task = &step.task;
    if reads_across_hosts(task) {
        return true;
    }
    let (text, bare) = task_strings(task);
    // Every word, for the names: a name in a text that is not an expression costs a barrier at
    // worst. Only the words of expressions for the dynamic reads, or every task called
    // "Include OS-specific vars" would be one.
    let mut named = BTreeSet::new();
    let mut read = BTreeSet::new();
    for s in &text {
        words(s, &mut named);
        for expr in expressions(s) {
            words(expr, &mut read);
        }
    }
    for s in &bare {
        words(s, &mut named);
        words(s, &mut read);
    }
    let lookup = task.loop_with.as_deref().map(short_name);
    DYNAMIC_READS
        .iter()
        .any(|w| read.contains(*w) || lookup == Some(*w))
        || !named.is_disjoint(crossing)
        || (short_name(&task.module) == "template"
            && template_crosses(step, crossing, playbook_dir))
}

/// Whether the file a `template` task renders reads another host. A `src` that is itself a
/// template, a file that is not there or not text, and a file pulling in another are all taken
/// to read one: the scan cannot see what they hold.
fn template_crosses(step: &Step, crossing: &BTreeSet<String>, playbook_dir: &Path) -> bool {
    let Some(src) = step.task.args.get("src").and_then(Value::as_str) else {
        return true;
    };
    if Templar::is_template(src) {
        return true;
    }
    let body =
        crate::action_plugins::files::search_paths(&step.origin, playbook_dir, "templates", src)
            .into_iter()
            .find(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok());
    let Some(body) = body else {
        return true;
    };
    if mentions(&body) {
        return true;
    }
    let mut read = BTreeSet::new();
    for expr in expressions(&body) {
        let first = expr
            .trim_start_matches(['-', '+'])
            .split_whitespace()
            .next();
        if first.is_some_and(|w| TEMPLATE_PULLS.contains(&w)) {
            return true;
        }
        words(expr, &mut read);
    }
    DYNAMIC_READS.iter().any(|w| read.contains(*w)) || !read.is_disjoint(crossing)
}
