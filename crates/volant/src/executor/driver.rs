// SPDX-License-Identifier: GPL-3.0-or-later
//! One host's run through a play, from its first step to the moment it leaves the batch.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use volant_protocol::modules::{arg_bool, is_builtin, short_name};
use volant_protocol::{BatchOutcome, TaskResult};

use crate::action_plugins::Kind;
use crate::agent::{AgentLink, AgentSource};
use crate::compile::{
    Compiled, Step, StepKind, after, after_failure, after_pending, first, rescue_target,
};
use crate::inventory::Host;
use crate::playbook::PlayTask;
use crate::profile::{Phase, Profile, TaskRecord, micros};
use crate::python::ModulePayload;
use crate::render::Dump;
use crate::template::{Templar, TemplateError};
use crate::transport::{ConnectError, Escalation, Transport};
use crate::vars::{HostVars, VarStore};

use super::coordinator::{Event, Progress, escalated_links, render_name};
use super::include::{report_include, resolve_include};
use super::prepare::{Item, PlayPlan, Prepared, prepare, retry_name};
use super::report::report_task;
use super::run::{
    Attempt, PluginStart, Relinker, Retry, chosen_interpreter, fact_targets, failed_task_value,
    finish, judge_attempt, notify, python_for, record_facts, record_registered,
    requested_interpreter, retry_plan, reuse_or_connect, run_agent_batch, run_local,
    run_plugin_attempts, running_host_vars, step_tasks, take_warnings, unresolved_notify,
    wait_or_stop,
};
use super::{LinkKey, RunOptions};

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
fn is_boundary(c: &Compiled, pos: usize, strict: bool) -> bool {
    if strict || c.steps[pos].task.barrier() {
        return true;
    }
    // A list nobody marked is a list where every step is a boundary.
    c.crosses.len() != c.steps.len() || c.crosses[pos]
}

/// Whether the hosts meet in front of every step: `[volant] batching` is off, or a fact the
/// playbook wrote while the run went is still a template (see [`VarStore::templated_facts`]).
/// Asked again at every step, because the second half can turn on at any of them.
fn strict(batching: bool, store: &Mutex<VarStore>) -> bool {
    !batching || store.lock().expect("vars lock").templated_facts()
}

/// The builtin modules of ansible-core 2.19.12 that answer with `ansible_facts`: every file of
/// `ansible/modules/` and `ansible/plugins/action/` that sets the key, read off the source with
/// `grep -l ansible_facts` and checked one by one (the `dnf` action writes `pkg_mgr`; `package`,
/// `service`, `reboot` and `wait_for_connection` only read facts). `set_fact` and `include_vars`
/// run on the controller and never join a batch; they are here because they write facts all the
/// same.
const FACT_MODULES: [&str; 10] = [
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
fn writes_facts(task: &PlayTask) -> bool {
    !is_builtin(&task.module) || FACT_MODULES.contains(&short_name(&task.module))
}

/// Whether rendering a task calls a lookup: `lookup`, `query` or `q` in an expression, or a
/// `with_<lookup>` loop. A lookup reads the controller, a file or a command's output, at the
/// moment it renders.
fn calls_lookup(task: &PlayTask) -> bool {
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
fn reads_across_hosts(task: &PlayTask) -> bool {
    let (text, bare) = task_strings(task);
    text.iter().chain(&bare).any(|s| mentions(s))
}

/// A play's static variable definitions, as far as the barrier cares: for each name, whether its
/// value reads another host on its own, and every word its expressions name.
///
/// Names are pooled over every host, role and layer: a name that reads another host anywhere is
/// treated as reading one everywhere. That is wider than any one host's view and never narrower.
#[derive(Debug, Clone, Default)]
pub(super) struct Definitions(Vec<(String, bool, BTreeSet<String>)>);

impl Definitions {
    pub(super) fn add(&mut self, map: &Map<String, Value>) {
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
pub(super) fn mark_boundaries(c: &mut Compiled, base: &Definitions, playbook_dir: &Path) {
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

#[expect(
    clippy::too_many_arguments,
    reason = "the driver's whole context; drive_host has a single call site, spawned once per host by the loop above"
)]
pub(super) async fn drive_host(
    host: Host,
    plan: Arc<PlayPlan>,
    agents: AgentSource,
    options: RunOptions,
    templar: Arc<Templar>,
    store: Arc<Mutex<VarStore>>,
    verbosity: u8,
    existing: Vec<(LinkKey, AgentLink)>,
    forks: Arc<Semaphore>,
    progress: watch::Receiver<Progress>,
    tx: mpsc::Sender<Event>,
) {
    let name = host.name.clone();
    let mut driver = Driver {
        tx: &tx,
        name: &name,
        plan: &plan,
        progress,
        stop: options.stop.clone(),
        stop_broken: false,
        cleanup: None,
    };
    // One entry per target user on this host: the connecting user's own agent, and one more
    // for each `become_user` the play escalates to. Connections this play never uses itself
    // stay in here untouched and go straight back with `Finished`.
    let mut links: HashMap<LinkKey, AgentLink> = existing.into_iter().collect();
    // Which of them this play has already proved alive, with the host's reboot count when it
    // did: one liveness check per connection per play, and one more after each reboot.
    let mut checked: HashMap<LinkKey, u64> = HashMap::new();
    // Ansible's `forks`: one permit per host, held from the connection to the results of a
    // batch. It lives in this binding and releases itself when dropped, so no way out of this
    // function can leak it, whether that is a return, an error, a cancellation or a panic.
    let mut permit: Option<OwnedSemaphorePermit> = None;
    // Set when the host leaves the run without finishing: the message the recap shows, and
    // whether the task it happened under censors its output.
    let mut unreachable: Option<String> = None;
    let mut unreachable_censored = false;
    // The delegate of the task whose batch could not be reached, for the `[h2 -> h1]` the
    // `UNREACHABLE!` line carries. Measured on ansible-core 2.19.12: the delegating host is the
    // one the recap counts, and the delegate only shows in the line.
    let mut unreachable_delegate: Option<String> = None;
    let mut failed = false;
    // Set when this host leaves the play with nothing to show for it: the one runner of a
    // `run_once` step failed, so this host never ran the task and has no line, and the run
    // still has to exit 2 for it. Measured on ansible-core 2.19.12.
    let mut silent_failure = false;
    // Not always step 0: a block with an empty `block:` list lays its rescue out there, and
    // nothing has failed. The steps in front of the first one are stepped over like any other.
    let mut pos = first(&plan.steps());
    // An interrupt here leaves `pos` where it is; the run loop's own `stop` test below is what
    // ends the play for this host, and the `Finished` at the end of this function still goes.
    if let Some(grown) = driver.stepped_over(0..pos).await {
        pos += grown;
    }
    let mut batch_id: u64 = 0;
    // The handlers this host has asked for and not yet run, as indices into `compiled.handlers`.
    // Never twice: measured, a handler notified by two tasks runs once.
    let mut notified: Vec<usize> = Vec::new();
    // Set while this host is walking the handler steps of one flush point. What it is for is the
    // end of that walk: measured on ansible-core 2.19.12, a handler that notifies a handler
    // **defined before it** runs nothing - neither in that flush nor in the next one - so the
    // notifications a flush raised die with it.
    let mut in_flush = false;
    // Set when a host that failed carries on for its handlers alone, which is what
    // `force_handlers` asks for. Every step but a notified handler is reported and not run.
    let mut handlers_only = false;
    // The step a terminal failure was raised at, read once the host has finished whatever
    // cleanup it owed: it is where `force_handlers` picks the walk back up.
    let mut failed_index: Option<usize> = None;
    // The step a failure was just reported at, with the result it failed with. The `always`
    // section this host is draining on its way out is the other half of a failure's state, and
    // it lives on `Driver::cleanup`; the coordinator still knows one thing about a host, that
    // it failed.
    let mut failed_at: Option<(usize, TaskResult)> = None;

    'run: loop {
        let c = plan.steps();
        let n = c.steps.len();
        // The fork permit goes back before either failure arm below waits at a splice point: the
        // step loop's own release reads the success successor and never the range a host that
        // has just failed steps over. Only on the failure paths, so a batch that ended cleanly
        // still carries its permit into the next one.
        //
        // The escalated links go back here as well, and here alone among the releases. A host
        // that has just failed is on its way through a rescue or an `always` and out of the
        // play, and what it escalated to is far less likely to be what it needs next than it is
        // on the straight path; it is also the state most likely to be left waiting a long time
        // at a splice point. So this is where an escalated link stops being worth its process on
        // the target.
        if failed_at.is_some() || failed_index.is_some() {
            permit = None;
            for key in escalated_links(&links) {
                if let Some(link) = links.remove(&key) {
                    tokio::spawn(link.shutdown());
                }
            }
        }
        if let Some((index, result)) = failed_at.take() {
            // A handler that failed under `force_handlers` stops the rest: measured on
            // ansible-core 2.19.12, `good handler` does not run behind a `bad handler` that
            // failed, with the flag and without it.
            if handlers_only {
                break 'run;
            }
            // A `rescue` takes this failure: the host jumps into it and stays in the play,
            // which is why `failed` was never set for it. The two variables the recovery
            // reads are written on the way in, and they outlive the block and the play -
            // measured, a task after the block and a task in the next play both still read
            // `ansible_failed_task`.
            if let Some(next) = rescue_target(&c, index) {
                let task = &c.steps[index].task;
                {
                    let mut vars = store.lock().expect("vars lock");
                    vars.set_untrusted_fact(&name, "ansible_failed_task", failed_task_value(task));
                    vars.set_untrusted_fact(
                        &name,
                        "ansible_failed_result",
                        Value::Object(result.0.clone()),
                    );
                }
                let Some(grown) = driver.stepped_over(index + 1..next).await else {
                    break 'run;
                };
                // A rescue written **inside** the `always` this host was draining leaves the
                // drain where it is, moved by whatever the splices grew in front of its end;
                // one written outside ends it. Measured: `rest of the always` runs behind a
                // rescued block written in a cleanup, and stops as soon as an include between
                // the failure and that rescue grows the list.
                driver.cleanup = match driver.cleanup {
                    Some(end) if next < end => Some(end + grown),
                    _ => None,
                };
                pos = next + grown;
            } else {
                // Measured on ansible-core 2.19.12: a task failing inside a nested block runs
                // the inner `always`, then the outer one, and only then leaves the play. A
                // `cleanup` already in hand is left where it is: a failure raised while
                // draining one `always` still has to finish leaving through the ones outside.
                failed_index = Some(index);
                match after_failure(&c, index) {
                    Some((next, end)) => {
                        let Some(grown) = driver.stepped_over(index + 1..next).await else {
                            break 'run;
                        };
                        pos = next + grown;
                        driver.cleanup = Some(end + grown);
                    }
                    // Nothing left to clean up. Where the host goes from here is the single
                    // test below, so `force_handlers` changes it in one place rather than
                    // two.
                    None => pos = n,
                }
            }
        }
        // The host has run everything it owed and failed on the way. `force_handlers` sends it
        // back over the rest of the play for its handlers alone: measured, the notified handler
        // runs and the recap still counts the failure (`h2 ok=7 failed=1` against `ok=6` without
        // the flag).
        if pos >= n
            && plan.force_handlers
            && !handlers_only
            && let Some(index) = failed_index.take()
        {
            handlers_only = true;
            driver.cleanup = None;
            let Some(next) = driver.advance(index).await else {
                break 'run;
            };
            pos = next;
            continue 'run;
        }
        if pos >= n || driver.stopped() {
            break 'run;
        }
        // Collect a batch of remote tasks up to the next boundary; report skips and run local
        // tasks as they come, in order.
        let mut batch: Vec<(usize, Vec<Item>)> = Vec::new();
        // The module payload of each step of the batch that has one, by step. Per task rather
        // than per item, because the module is the task's: a loop varies the arguments. Empty
        // for every batch this release runs today, a Python module being refused before the
        // first connection.
        let mut payloads: HashMap<usize, (Box<ModulePayload>, Option<String>)> = HashMap::new();
        // How long each step of the batch took to render, for the profile.
        let mut prepare_micros: HashMap<usize, u64> = HashMap::new();
        // The escalation every task of the batch shares. A batch is one message to one agent,
        // so it cannot span two target users.
        let mut batch_escalation: Option<Escalation> = None;
        // The host every task of the batch runs on, when a `delegate_to` moved it off this one:
        // the name for the `[h1 -> h3]` its lines carry, and the host the link is opened to and
        // keyed by. One batch is one link, so a second delegate ends the batch like a second
        // target user does.
        let mut batch_delegate: Option<String> = None;
        // The connection every task of the batch shares, resolved from each task's own effective
        // variables. One batch is one link, so a task that resolves a different address, port,
        // user or key ends the batch the way a second target user does. Set on every push, so it
        // holds a value exactly when `batch` does.
        let mut batch_transport: Option<Transport> = None;
        // Set when the batch's one task retries. A retried task is alone in its batch, so this
        // says how the whole batch runs: item by item, attempt by attempt, rather than in one
        // trip to the agent.
        let mut batch_retry: Option<Retry> = None;
        // Set when the batch's one task is backed by an action plugin, which is alone in its
        // batch the way a retried task is.
        let mut batch_action: Option<PluginBatch> = None;
        let mut deferred_error: Option<(usize, TemplateError)> = None;
        // The last step of the batch, when where the host goes after it is not yet decided.
        // See the `Prepared::Remote` arm: a step a rescue would catch cannot say what it steps
        // over until its own result is in.
        let mut undecided: Option<usize> = None;
        while pos < n {
            // The list grew behind this host at a flush point it stepped over, so every index
            // past that point has moved: go back and read the list again rather than walk the
            // one it used to be. Nothing collected so far moved - a flush ends a batch, so every
            // index in `batch` sits in front of it.
            if plan.steps().steps.len() != n {
                break;
            }
            let step = &c.steps[pos];
            let task = &step.task;
            // A permit kept from the batch before goes back in front of every wait this loop can
            // reach, because a permit held across one is a permit the hosts that have to reach
            // that same point cannot have. The batch being empty is what says this is the kept
            // permit and not the one this batch works under: no wait is reached with a batch in
            // hand.
            //
            // The escalated link does **not** go back with it. A permit is the run's own token
            // and another host is waiting for it; an escalated link is a process and a
            // connection on this host alone, which no other host is waiting for, so holding one
            // across a barrier delays nobody. Releasing it costs an `ssh`, a `sudo` and a
            // handshake at the next escalated task, and under the strict `linear` every
            // task raises a barrier. What the links are bounded by is `keep_links` at the end of
            // the play, the failure arm at the top of this loop, and the rule below that a host
            // holds at most one of them at a time.
            if batch.is_empty()
                && permit.is_some()
                && (is_boundary(&c, pos, strict(options.batching, &store))
                    || crate::compile::is_splice_point(&step.kind)
                    || driver.steps_over_a_splice_point(&c, pos))
            {
                permit = None;
            }
            // A step an include spliced in for other hosts. This host reports it and shows
            // nothing, exactly as it does for a handler it never notified: the coordinator's
            // barriers open on hosts that have passed a step, not on hosts that had a reason to
            // run it. Measured on ansible-core 2.19.12 with an include a `when` left out for one
            // of two hosts - the other's tasks run with its line alone under their banners.
            let masked_out = step
                .hosts
                .as_ref()
                .is_some_and(|only| !only.iter().any(|h| h == &name));
            // A handler nothing notified is a step this host has nothing to do at: it reports
            // that it is done with it, shows nothing and moves on. Measured on ansible-core
            // 2.19.12 with two hosts and one notifying: the banner shows once, with the line of
            // the host that notified and nothing for the other, whose recap counts no `ok`.
            if let StepKind::Handler(i) = step.kind
                && !notified.contains(&i)
            {
                if !batch.is_empty() {
                    break;
                }
                driver.task_done(pos).await;
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // A step the hosts of the batch meet in front of: `run_once`, or a text naming another
            // host's state. In front of the flush, include and mask arms rather than behind them,
            // because a host that walked into one of those without stopping here would report a
            // step the others have not reached, and every wait in this file opens on the
            // coordinator's frontier.
            if is_boundary(&c, pos, strict(options.batching, &store)) && pos > 0 {
                if !batch.is_empty() {
                    break;
                }
                let waiting = Instant::now();
                let waited = driver.wait_for_barrier(pos).await;
                options
                    .profile
                    .phase(Some(&name), Phase::BarrierWait, micros(waiting));
                if waited.is_none() {
                    break 'run;
                }
            }
            // Who runs a `run_once` step: read, never decided here, because one value written
            // once is what makes every host agree on it. See `Progress::elected`. With `serial`
            // each batch elects its own, which falls out of the coordinator's `live_hosts` being
            // the batch's list and not the play's.
            let runner = driver.progress.borrow().elected.get(&pos).cloned();
            // Measured on ansible-core 2.19.12: `changed: [h1]` alone under the banner, the
            // registered variable and the facts readable on h2 as well, and h2 counting no `ok`
            // for it. Measured again with an include a `when` kept one of three hosts out of:
            // the two the mask holds both read the registered value back, and the third runs
            // only what follows.
            let follower = task.bypasses_host_loop()
                && !handlers_only
                && runner.as_deref().is_some_and(|h| h != name);
            // Leaving a flush point's handlers behind: every index the flush was asked for goes,
            // reached or not, so a handler runs once per notification. Measured on ansible-core
            // 2.19.12, both halves - `first handler` notified twice runs once and does **not**
            // come back at the next flush, and a handler notified by a handler defined **before**
            // it runs in neither flush.
            if in_flush && !matches!(step.kind, StepKind::Handler(_)) {
                notified.clear();
                in_flush = false;
            }
            // A flush point. This is the one step whose successors are not known yet, so the
            // host reports it and then waits for the coordinator to put the handler steps in
            // behind it. Reading the list before that would walk straight past them.
            if let StepKind::Flush { explicit } = step.kind {
                if !batch.is_empty() {
                    break;
                }
                // Measured: an explicit `meta: flush_handlers` shows one `TASK [meta]` per live
                // host with nothing under it, and the three the compiler adds show nothing at
                // all. A host already out of the play shows nothing either.
                if explicit && !handlers_only && !masked_out {
                    driver.banner(pos).await;
                }
                driver.task_done(pos).await;
                // `n` is the length of the list this host is walking, read before the word it
                // just sent could let the coordinator grow it. See `wait_for_splice`.
                if driver.wait_for_splice(pos, n).await.is_none() {
                    break 'run;
                }
                // A host the flush's own mask leaves out reports the step and waits for the
                // splice like everyone else, but it has not performed a flush: the handler steps
                // behind it carry that same mask, so it runs none of them, and the line below
                // would otherwise throw away the notifications it is still carrying to the next
                // flush it does reach.
                if !masked_out {
                    in_flush = true;
                }
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                break;
            }
            // An include statement. Like a flush point its successors are not known yet, so the
            // shape is the same: report, wait for the coordinator to publish the splice, and only
            // then read the list again. The batch has to be empty to get here, which is the
            // liveness half of the splice rule - a host blocked at an index while a step in front
            // of it is still unreported waits for an index the coordinator can never reach.
            if let StepKind::Include(kind) = step.kind {
                if !batch.is_empty() {
                    break;
                }
                let live = driver.progress.borrow().clone();
                // A host outside the mask, running for its handlers alone, or that lost the
                // `run_once` election asks for nothing and shows nothing, and still reports the
                // step and waits: the steps go in behind this index for everyone's list. Measured
                // on ansible-core 2.19.12, `include_tasks` under `run_once: true`: `included:
                // inc.yml for h1` names the one host, the tasks run for h1 alone, h2 counts none.
                let asked = if masked_out || handlers_only || follower {
                    Vec::new()
                } else {
                    let mut warnings = Vec::new();
                    let (groups, shown) = resolve_include(
                        &c,
                        step,
                        kind,
                        &name,
                        &plan,
                        &live,
                        &templar,
                        &store,
                        &options.defaults,
                        &mut warnings,
                    );
                    for message in warnings {
                        let _ = tx
                            .send(Event::Warning {
                                host: name.clone(),
                                index: pos,
                                message,
                                censored: false,
                            })
                            .await;
                    }
                    let rescuable = !handlers_only && rescue_target(&c, pos).is_some();
                    if let Some(result) =
                        report_include(&tx, &name, pos, task, &shown, rescuable, groups.is_empty())
                            .await
                    {
                        failed |= !rescuable;
                        failed_at = Some((pos, result));
                    }
                    groups
                };
                let _ = tx
                    .send(Event::Include {
                        host: name.clone(),
                        index: pos,
                        groups: asked,
                    })
                    .await;
                driver.task_done(pos).await;
                // Waited for even by a host whose own include just failed: the splice grows every
                // index past this one, and `rescue_target` and `after` are about to be asked for
                // this step on the list the splice leaves behind. A host that skipped the wait
                // would jump to a target computed against the list as it used to be.
                if driver.wait_for_splice(pos, n).await.is_none() {
                    break 'run;
                }
                if failed_at.is_some() {
                    break;
                }
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // A host carrying on for its handlers alone runs nothing else: it reports each step
            // it walks past so the others' barriers open, and shows nothing for it. A handler
            // is the exception, and a handler it never notified has already been reported and
            // stepped over above, so one reaching here is one it asked for.
            if handlers_only && !matches!(step.kind, StepKind::Handler(_)) {
                if !batch.is_empty() {
                    break;
                }
                driver.task_done(pos).await;
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // A step an include brought in for other hosts: reported, shown nothing, the way a
            // handler this host never notified is. After the flush and include arms on purpose,
            // because those are splice points a host outside the mask still has to report and
            // wait at. A `run_once` step takes the follower arm below instead: reporting it here
            // would publish a verdict for the hosts waiting on a runner that has not run.
            if masked_out && !follower {
                if !batch.is_empty() {
                    break;
                }
                driver.task_done(pos).await;
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            // A host that lost the `run_once` election waits here for the coordinator to publish
            // what the runner made of the step: what that host registered or set as a fact is
            // written for the whole batch, and reading it a moment too early reads nothing. Below
            // the include and flush arms on purpose - a follower still reports those and waits
            // for the splice, it just asks for nothing.
            if follower {
                if !batch.is_empty() {
                    break;
                }
                let runner = runner.clone().unwrap_or_default();
                let Some(verdict) = driver.wait_for_run_once(&runner, pos).await else {
                    break 'run;
                };
                match verdict {
                    // Measured on ansible-core 2.19.12: a `run_once` task that failed takes
                    // every other host of the play out of it, with no line and no recap entry
                    // of their own - they ran nothing - while the run still exits 2. It holds
                    // with a `rescue` around the task as well: the one host that ran it is
                    // rescued (`rescued=1`) and the others still leave.
                    RunOnce::Failed => {
                        failed = true;
                        silent_failure = true;
                        break 'run;
                    }
                    // The one runner left the play without a verdict, which is what an
                    // unreachable host does. Measured: `fatal: [h1]: UNREACHABLE!` with no
                    // arrow, h2 absent from the recap, exit **4** and not 6 - so this departure
                    // counts for nothing of its own.
                    RunOnce::Gone => {
                        failed = true;
                        break 'run;
                    }
                    RunOnce::Done => {}
                }
                driver.task_done(pos).await;
                let Some(next) = driver.advance(pos).await else {
                    break 'run;
                };
                pos = next;
                continue;
            }
            let live = driver.progress.borrow().clone();
            // Where a registered variable lands. A `run_once` step is run by one host for the
            // whole batch, so what it registered is written for every live host of the batch:
            // measured on ansible-core 2.19.12, `register: o` under `run_once: true` on h1
            // reads back as `o.stdout` on h2 as well. Every other step writes for its own host.
            let register_hosts: Vec<String> = fact_targets(task, &name, &live.live_hosts);
            // A step calling a lookup starts its own batch. Every step of a batch is rendered
            // before the batch goes out, so a lookup rendered behind a batch in hand would read
            // the controller before the tasks in front of it had run - a file one of them
            // writes, a command's output - where the strict `linear` reads it after them.
            if !batch.is_empty() && calls_lookup(task) {
                break;
            }
            // The banner's name, rendered here, at the step's own point, against the variables
            // this host has now; the coordinator prints it rather than rendering its own later.
            if Templar::is_template(&task.name) {
                let spoken = render_name(step, &name, &plan, &live, &templar, &store);
                let _ = tx
                    .send(Event::Named {
                        host: name.clone(),
                        index: pos,
                        name: spoken,
                    })
                    .await;
            }
            let mut warnings = Vec::new();
            let preparing = Instant::now();
            let prepared = prepare(
                step,
                &name,
                &plan,
                &live,
                &templar,
                &store,
                &options.defaults,
                &mut warnings,
            );
            let took = micros(preparing);
            options.profile.phase(Some(&name), Phase::Prepare, took);
            prepare_micros.insert(pos, took);
            // Not censored: measured on ansible-core 2.19.12, the one warning `prepare` can
            // raise quotes the playbook's own `environment` source and never a rendered value,
            // and the reference leaves it in plain sight under `no_log`.
            for message in warnings {
                let _ = tx
                    .send(Event::Warning {
                        host: name.clone(),
                        index: pos,
                        message,
                        censored: false,
                    })
                    .await;
            }
            match prepared {
                Err(err) => {
                    deferred_error = Some((pos, err));
                    break;
                }
                Ok(Prepared::Skipped(items)) => {
                    if !batch.is_empty() {
                        break;
                    }
                    let labels: Vec<Option<String>> =
                        items.iter().map(|i| i.label.clone()).collect();
                    let results: Vec<(Option<Value>, TaskResult)> = items
                        .into_iter()
                        .map(|i| (i.element, i.skipped.unwrap_or_default()))
                        .collect();
                    // A skipped task fails nothing, so no rescue is in question for it, and it
                    // made no attempt to retry.
                    // No delegate on a skipped task: measured on ansible-core 2.19.12, a
                    // `delegate_to` a `when` left out prints `skipping: [h1]` with no arrow.
                    report_task(
                        &tx,
                        &name,
                        pos,
                        task,
                        &results,
                        &labels,
                        &[],
                        &[],
                        &[],
                        Dump::No,
                        false,
                        None,
                    )
                    .await;
                    record_registered(
                        &mut store.lock().expect("vars lock"),
                        task,
                        &register_hosts,
                        &results,
                        &[],
                    );
                    let Some(next) = driver.advance(pos).await else {
                        break 'run;
                    };
                    pos = next;
                }
                // A `meta` asks the engine for something rather than the host: it shows a
                // banner, reports nothing and moves on. Every action that reaches here is one
                // this release honours by doing nothing; the pre-flight refused the rest.
                Ok(_) if step.kind == StepKind::Meta => {
                    if !batch.is_empty() {
                        break;
                    }
                    driver.banner(pos).await;
                    driver.task_done(pos).await;
                    let Some(next) = driver.advance(pos).await else {
                        break 'run;
                    };
                    pos = next;
                }
                Ok(Prepared::Local(items, delegate)) => {
                    if !batch.is_empty() {
                        break;
                    }
                    // A controller-side module runs here whatever `delegate_to` says - measured
                    // on ansible-core 2.19.12, a delegated `set_fact` and a delegated `debug`
                    // both run on the controller - so the delegate decides two things and no
                    // more: the name the line shows, and, under `delegate_facts`, whose facts
                    // the module writes.
                    let fact_hosts: Vec<String> = match (task.delegates_facts(), &delegate) {
                        (true, Some(to)) => vec![to.clone()],
                        _ => register_hosts.clone(),
                    };
                    let retry = match retry_plan(task, items.first(), &templar) {
                        Ok(retry) => retry,
                        Err(err) => {
                            deferred_error = Some((pos, err));
                            break;
                        }
                    };
                    let mut results = Vec::new();
                    let mut labels = Vec::new();
                    let mut lefts: Vec<Vec<u32>> = Vec::new();
                    let mut names: Vec<String> = Vec::new();
                    for item in &items {
                        names.push(retry_name(task, &item.vars, &templar));
                        let mut mine = Vec::new();
                        let r = if let Some(s) = &item.skipped {
                            s.clone()
                        } else {
                            let mut attempt = 0;
                            loop {
                                attempt += 1;
                                // The `timeout` keyword holds on the controller as it does
                                // on the agent, per attempt, and zero is no timeout: measured
                                // on ansible-core 2.19.12. A `pause` is the one local module
                                // that can run into it.
                                let ran = run_local(
                                    task,
                                    item,
                                    step,
                                    &fact_hosts,
                                    &templar,
                                    &store,
                                    verbosity,
                                    &mut driver.stop,
                                );
                                let ran = match task.timeout.filter(|t| *t > 0) {
                                    Some(t) => tokio::time::timeout(Duration::from_secs(t), ran)
                                        .await
                                        .unwrap_or_else(|_| Some(TaskResult::timed_out(t))),
                                    None => ran.await,
                                };
                                // A pause the run's stop ended: no line and no count, like
                                // every other wait the stop ends.
                                let Some(ran) = ran else {
                                    break 'run;
                                };
                                let Some(retry) = &retry else {
                                    break finish(task, item, ran, &templar);
                                };
                                match judge_attempt(task, item, ran, attempt, retry, &templar) {
                                    Attempt::Done(r) => break r,
                                    Attempt::Again(left) => mine.push(left),
                                }
                                if driver.sleep_between(retry.delay).await.is_none() {
                                    break 'run;
                                }
                            }
                        };
                        results.push((item.element.clone(), r));
                        labels.push(item.label.clone());
                        lefts.push(mine);
                    }
                    if let Some(reason) = unresolved_notify(&c, task, &results) {
                        options.abort.raise(reason);
                        break 'run;
                    }
                    let warnings = take_warnings(&mut results);
                    for message in &warnings {
                        let _ = tx
                            .send(Event::Warning {
                                host: name.clone(),
                                index: pos,
                                message: message.clone(),
                                censored: task.censors(),
                            })
                            .await;
                    }
                    record_registered(
                        &mut store.lock().expect("vars lock"),
                        task,
                        &register_hosts,
                        &results,
                        &warnings,
                    );
                    let rescuable = !handlers_only && rescue_target(&c, pos).is_some();
                    // A censored `debug` shows nothing at all at verbosity 0 and its censored
                    // body from `-v` on, measured: the dump is what puts the body on the line,
                    // so it is the dump that goes. An `assert` dumps its whole result unless it
                    // is `quiet`, measured on ansible-core 2.19.12, and `quiet` is the one
                    // thing that argument does.
                    let dump = match short_name(&task.module) {
                        "debug" if !(task.censors() && verbosity == 0) => Dump::Debug,
                        "assert"
                            if !items.first().is_some_and(|i| {
                                i.args.get("quiet").and_then(arg_bool) == Some(true)
                            }) =>
                        {
                            Dump::Whole
                        }
                        _ => Dump::No,
                    };
                    if let Some(result) = report_task(
                        &tx,
                        &name,
                        pos,
                        task,
                        &results,
                        &labels,
                        &items.iter().map(|i| i.ignore_errors).collect::<Vec<_>>(),
                        &lefts,
                        &names,
                        dump,
                        rescuable,
                        delegate.as_deref(),
                    )
                    .await
                    {
                        failed |= !rescuable;
                        failed_at = Some((pos, result));
                        break;
                    }
                    notify(&c, task, &results, &mut notified);
                    let Some(next) = driver.advance(pos).await else {
                        break 'run;
                    };
                    pos = next;
                }
                Ok(Prepared::Remote(items, escalation, delegate, payload, action)) => {
                    // A different target user is a different agent on the host, a different
                    // delegate is a different host entirely, and a different connection is a
                    // different machine even under one name, so any of the three ends the batch
                    // and the next one opens its own link. `pos` does not move, so this task is
                    // the first of that batch.
                    //
                    // The connection is resolved here, from this task's own effective variables,
                    // and not once per batch afterwards: a task's `vars:` changes `ansible_host`,
                    // `ansible_port` or `ansible_connection` without being a boundary of any
                    // other kind, so a batch built from its first task's view alone would run it
                    // on the machine the first task chose.
                    let delegate_name = delegate.as_ref().map(|(name, _)| name.clone());
                    let target = delegate_name.clone().unwrap_or_else(|| name.clone());
                    let target_vars = running_host_vars(delegate.as_ref(), &items);
                    // The host's own map is enough here: every connection setting is a name a
                    // gather strips, so none of them is ever in the facts layer beside it.
                    let transport =
                        match Transport::for_vars(&target, &target_vars.map, &options.defaults) {
                            Ok(transport) => transport.shared(
                                options.control_dir.as_deref(),
                                &name,
                                delegate_name.as_deref(),
                            ),
                            // The tasks in hand ran under a connection that was resolved; this
                            // one is the next batch's first, where the same failure ends the
                            // host with its own message.
                            Err(_) if !batch.is_empty() => break,
                            // The batch this task would have opened never formed, so the two
                            // fields the `UNREACHABLE!` line reads are set here rather than
                            // below: the arrow names the delegate this task asked for, and
                            // `no_log` censors the reason.
                            Err(err) => {
                                unreachable_censored = task.censors();
                                unreachable_delegate = delegate_name;
                                unreachable = Some(format!("{err:#}"));
                                break 'run;
                            }
                        };
                    if !batch.is_empty()
                        && (escalation != batch_escalation
                            || delegate_name != batch_delegate
                            || Some(&transport) != batch_transport.as_ref())
                    {
                        break;
                    }
                    let retry = match retry_plan(task, items.first(), &templar) {
                        Ok(retry) => retry,
                        Err(err) => {
                            deferred_error = Some((pos, err));
                            break;
                        }
                    };
                    // A retried task runs its items one at a time, each on its own trip to the
                    // agent, so it is alone in its batch: the tasks in hand go out first and
                    // this one opens the next batch. It ends that batch too, through `boundary`.
                    // A task an action plugin backs does the same, one trip per sub-task.
                    if (retry.is_some() || action.is_some()) && !batch.is_empty() {
                        break;
                    }
                    batch_action = action.map(|kind| PluginBatch {
                        kind,
                        asked: requested_interpreter(target_vars),
                        delegate: delegate.clone(),
                    });
                    batch_escalation = escalation;
                    batch_delegate = delegate_name;
                    batch_transport = Some(transport);
                    batch_retry = retry;
                    // A looping task ends the batch because its items travel with
                    // `ignore_errors` set, so the agent runs all of them the way Ansible does.
                    // Only `report_task` may decide the task failed, from the aggregate, and
                    // nothing behind it in the same batch is allowed to run before it has.
                    //
                    // A task that writes facts ends it as well: every task behind it in the batch
                    // is rendered now, before those facts exist, and one that reads them would
                    // run with nothing or with the values from before.
                    let boundary = task.register.is_some()
                        || writes_facts(task)
                        || task.loop_items.is_some()
                        || !task.changed_when.is_empty()
                        || !task.failed_when.is_empty()
                        || batch_retry.is_some()
                        || batch_action.is_some();
                    if let Some(module) = payload {
                        // Off the same variables the connection was resolved from, which is the
                        // point: the module runs where the link goes, so its interpreter has to
                        // come from that host and not from the one delegating the task. What the
                        // value is worth against the agent's list is settled once the link is up.
                        let asked = requested_interpreter(target_vars);
                        payloads.insert(pos, (module, asked));
                    }
                    batch.push((pos, items));
                    // Where this host goes after a step a `rescue` would catch depends on how that
                    // step ends, so the batch stops here: moving `pos` now would step over that
                    // very rescue and tell the coordinator so. A flush point between here and
                    // where the step leads ends the batch for its own reason - `advance` would
                    // wait for a splice the coordinator cannot publish without this step's word.
                    if rescue_target(&c, pos).is_some() || driver.steps_over_a_splice_point(&c, pos)
                    {
                        undecided = Some(pos);
                        break;
                    }
                    let Some(next) = driver.advance(pos).await else {
                        break 'run;
                    };
                    pos = next;
                    if boundary {
                        break;
                    }
                }
            }
        }

        if !batch.is_empty() {
            // Every way this batch can end the host's run reports the batch's first task's
            // `no_log`: the connection is opened for that task, and each of the four failures
            // below happens with it still unreported. Measured on ansible-core 2.19.12, the
            // reference censors the `UNREACHABLE!` line of a `no_log` task, reason and all.
            unreachable_censored = c.steps[batch[0].0].task.censors();
            unreachable_delegate = batch_delegate.clone();
            if permit.is_none() {
                if let Ok(p) = Arc::clone(&forks).acquire_owned().await {
                    permit = Some(p);
                } else {
                    // Nothing in this run closes the semaphore, so this is a bug rather than
                    // a shutdown. Reporting it beats returning as if the host had run.
                    unreachable = Some("the run's fork limit is gone".to_string());
                    break 'run;
                }
            }
            // The delegate's own connection, never the delegating host's. Measured on
            // ansible-core 2.19.12: a task delegated away from a host that answers nothing runs
            // perfectly well, so the delegating host's link is not opened for it at all.
            let target = batch_delegate.clone().unwrap_or_else(|| name.clone());
            // Resolved task by task inside the loop above, from each task's own effective
            // variables, and part of what ends the batch - so every task here shares this one
            // connection rather than inheriting the first task's. Set on every push, so the
            // `else` is unreachable and says so rather than guessing a connection.
            let Some(transport) = batch_transport.clone() else {
                unreachable = Some("the batch lost its resolved connection".to_string());
                break 'run;
            };
            let key = LinkKey {
                host: target,
                become_user: batch_escalation.as_ref().map(|e| e.user.clone()),
                transport,
            };
            // One escalated link at a time, now that they are no longer released at every
            // barrier: a batch escalating differently from the one in hand - another
            // `become_user`, another transport - retires the one it is replacing rather than
            // adding to it. So a host holds its own link plus at most one escalated link
            // whatever the number of users the play names, which is what the run was measured
            // for. Closed off-task: `shutdown` grants its agent two seconds and this batch has
            // no reason to wait for a connection it has finished with.
            if key.become_user.is_some() {
                for stale in escalated_links(&links).into_iter().filter(|k| k != &key) {
                    if let Some(link) = links.remove(&stale) {
                        tokio::spawn(link.shutdown());
                    }
                }
            }
            let connecting = Instant::now();
            let connected = reuse_or_connect(
                &mut links,
                &mut checked,
                &key,
                batch_escalation.as_ref(),
                &agents,
                &options,
            )
            .await;
            options
                .profile
                .phase(Some(&name), Phase::Connect, micros(connecting));
            let link = match connected {
                Ok(l) => {
                    options.profile.natives(&key.host, l.natives());
                    l
                }
                // The host answered and then refused to escalate, so this is the task failing and
                // not the host going away. `ignore_errors` is deliberately not honoured: the
                // batch never ran, and a run reporting success while having quietly skipped every
                // escalated task is the worst outcome here. Measured on ansible-core 2.19.12: a
                // refused `become_user` is a task failure, so a rescue takes it (`rescued=1`,
                // exit 0).
                Err(ConnectError::Become(msg)) => {
                    let index = batch[0].0;
                    let mut task = c.steps[index].task.clone();
                    task.ignore_errors = Some(crate::playbook::Flag::Fixed(false));
                    let results = vec![(None, TaskResult::failed_with(msg))];
                    let rescuable = !handlers_only && rescue_target(&c, index).is_some();
                    let result = report_task(
                        &tx,
                        &name,
                        index,
                        &task,
                        &results,
                        &[None],
                        &[],
                        &[],
                        &[],
                        Dump::No,
                        rescuable,
                        batch_delegate.as_deref(),
                    )
                    .await;
                    failed |= !rescuable;
                    failed_at = Some((index, result.unwrap_or_default()));
                    // Nothing ran, so the fork this host holds goes back before it carries on:
                    // the next round can block at a barrier or at a flush point, and a permit
                    // held across a wait is one the hosts that have to reach that same point
                    // cannot have. With `-f` under the number of live hosts, none of them ever
                    // would.
                    permit = None;
                    continue 'run;
                }
                Err(err) => {
                    unreachable = Some(err.to_string());
                    break 'run;
                }
            };
            let mut received: Vec<Vec<Option<TaskResult>>> = batch
                .iter()
                .map(|(_, items)| vec![None; items.len()])
                .collect();
            // The retry counts of each item of the retried task, empty for every other batch.
            let mut lefts: Vec<Vec<u32>> = batch
                .first()
                .map(|(_, items)| vec![Vec::new(); items.len()])
                .unwrap_or_default();
            // Each item's templated task name, parallel to `lefts` and empty the same way: the
            // retry loop below is the only place with the item's own vars in hand.
            let mut names: Vec<String> = Vec::new();
            // Whether `received` already holds results the conditions have been applied to. The
            // retry loop has to apply them itself, since `until` reads what they decided.
            let mut decided = false;
            // Lines the agent asked to show while this batch ran. They are shown in full, even
            // under `no_log`, for the same reason the `environment` warning is: every emitter of
            // `FromAgent::Log` carries transport or protocol text and none can carry task data -
            // the agent's own `main.rs` writes two of them before a batch exists, and `runner.rs`
            // writes the third about a frame it could not read, naming the message kind alone.
            //
            // Hiding them would only make a broken control channel say
            // `the output has been hidden...` where it used to say why, which is the one moment
            // an operator most needs the cause. `FromAgent::Log` carries no task index, so the
            // day one of them can quote a task this becomes the batch's own `no_log` - exact
            // under the strict barrier, erring towards hiding under `[volant] batching`.
            let mut logs: Vec<String> = Vec::new();
            // The step each result of a flat batch belongs to, by its position in the batch. The
            // plugin and retry paths send one step, so an empty list means `batch[0]`.
            let mut flat_steps: Vec<usize> = Vec::new();
            // First, so a task a plugin backs can never fall into a path below that would send
            // the plugin's own name to the agent as a module.
            let ended = if let Some(PluginBatch {
                kind,
                asked,
                delegate,
            }) = batch_action.take()
            {
                // The batch rule above keeps a plugin task alone. Should that ever break, the host
                // stops here naming it, rather than running `batch[0]` and dropping the rest.
                let (index, items) = match plugin_step(&batch) {
                    Ok(step) => step,
                    Err(msg) => {
                        unreachable = Some(msg);
                        break 'run;
                    }
                };
                let step = &c.steps[*index];
                let interpreters = link.interpreters().to_vec();
                // Out of the map while the plugin holds it, so that a plugin reconnecting can
                // drop the host's other links, and back in once the task is done, fresh or not.
                let Some(mut owned) = links.remove(&key) else {
                    unreachable = Some("the batch lost its connection".to_string());
                    break 'run;
                };
                let mut relink = Relinker {
                    links: &mut links,
                    checked: &mut checked,
                    reboots: &options.reboots,
                    key: &key,
                    escalation: batch_escalation.as_ref(),
                    agents: &agents,
                    defaults: &options.defaults,
                };
                let playbook_dir = store
                    .lock()
                    .expect("vars lock")
                    .playbook_dir()
                    .to_path_buf();
                let mut stopped = false;
                let mut outcome = Ok(BatchOutcome::Completed);
                // Under `retries`/`until` the attempts judge each result themselves, since
                // `until` reads what `changed_when` and `failed_when` decided.
                let retry = batch_retry.clone();
                if retry.is_some() {
                    decided = true;
                    names = items
                        .iter()
                        .map(|item| retry_name(&step.task, &item.vars, &templar))
                        .collect();
                }
                for (ii, item) in items.iter().enumerate() {
                    if item.skipped.is_some() {
                        continue;
                    }
                    let mut warnings = Vec::new();
                    let start = PluginStart {
                        kind,
                        running_vars: running_host_vars(
                            delegate.as_ref(),
                            std::slice::from_ref(item),
                        ),
                        delegated: delegate.is_some(),
                        escalated: batch_escalation.is_some(),
                        local: matches!(key.transport, Transport::Local),
                        templar: &templar,
                        origin: &step.origin,
                        playbook_dir: &playbook_dir,
                    };
                    let ran = run_plugin_attempts(
                        &mut owned,
                        &mut relink,
                        &name,
                        &mut batch_id,
                        &start,
                        &step.task,
                        item,
                        retry.as_ref(),
                        plan.python.as_deref(),
                        &interpreters,
                        asked.as_deref(),
                        &mut driver.stop,
                        &mut driver.stop_broken,
                        &mut logs,
                        &mut warnings,
                        &mut lefts[ii],
                    )
                    .await;
                    match ran {
                        // Stopped between two attempts: nothing more runs, as for every other
                        // wait the stop ends.
                        Ok(None) => {
                            stopped = true;
                            break;
                        }
                        Ok(Some(result)) => received[0][ii] = Some(result),
                        // The link lost or the run interrupted: this item has no result and the
                        // ones behind it do not run, exactly as for any batch the agent did not
                        // finish.
                        Err(ended) => {
                            outcome = ended;
                            break;
                        }
                    }
                    // Shown under this task, the way every other warning is. Not censored: the
                    // plugins raise nothing but the name of an argument they dropped.
                    for message in warnings {
                        let _ = tx
                            .send(Event::Warning {
                                host: name.clone(),
                                index: *index,
                                message,
                                censored: false,
                            })
                            .await;
                    }
                }
                links.insert(key.clone(), owned);
                if stopped {
                    if let Some(link) = links.get_mut(&key) {
                        file_timings(
                            link,
                            &options.profile,
                            &name,
                            &c,
                            &[],
                            batch[0].0,
                            &prepare_micros,
                        );
                    }
                    break 'run;
                }
                outcome
            } else if let Some(retry) = batch_retry.clone() {
                decided = true;
                let (index, items) = &batch[0];
                let task = &c.steps[*index].task;
                names = items
                    .iter()
                    .map(|item| retry_name(task, &item.vars, &templar))
                    .collect();
                let mut outcome = Ok(BatchOutcome::Completed);
                // Once for the step, never once per item: the payload and the interpreter are
                // read off the step and the host, and nothing about an item can change either.
                let chosen = payloads
                    .get(index)
                    .map(|(_, asked)| chosen_interpreter(asked.as_deref(), link.interpreters()));
                let python = python_for(
                    &task.module,
                    payloads.get(index).map(|(module, _)| &**module),
                    chosen.as_ref(),
                );
                // The same helper the batched path uses, for the same reason: a step that cannot
                // travel - no interpreter for its payload, or no payload for a module that needs
                // one - reports that per item from one place. Reported nowhere, an item leaves
                // its result `None`, the reporting loop below reads that as a host it never
                // reached, and the play finishes `ok` having run nothing.
                //
                // Item by item, in order, each one's attempts finished before the next one
                // starts: measured on ansible-core 2.19.12 with a two-item loop whose first item
                // passed and whose second needed two attempts - the retry lines of the second
                // sit between the two result lines, so the items do not retry together.
                'items: for (ii, built) in step_tasks(task, items, &python, &mut received[0]) {
                    let item = &items[ii];
                    let mut attempt = 0;
                    loop {
                        attempt += 1;
                        batch_id += 1;
                        let (mut flat, ended_one) = run_agent_batch(
                            link,
                            &name,
                            batch_id,
                            vec![built.clone()],
                            plan.python.as_deref(),
                            &[],
                            &mut driver.stop,
                            &mut driver.stop_broken,
                            &mut logs,
                        )
                        .await;
                        let raw = flat.pop().flatten();
                        if !matches!(
                            ended_one,
                            Ok(BatchOutcome::Completed | BatchOutcome::Failed { .. })
                        ) {
                            outcome = ended_one;
                            break 'items;
                        }
                        // The agent ended without a result for this item. Left unreported, the
                        // way the loop below leaves a task the agent never reached.
                        let Some(raw) = raw else { continue 'items };
                        match judge_attempt(task, item, raw, attempt, &retry, &templar) {
                            Attempt::Done(r) => {
                                received[0][ii] = Some(r);
                                continue 'items;
                            }
                            Attempt::Again(left) => lefts[ii].push(left),
                        }
                        if driver.sleep_between(retry.delay).await.is_none() {
                            file_timings(
                                link,
                                &options.profile,
                                &name,
                                &c,
                                &[],
                                batch[0].0,
                                &prepare_micros,
                            );
                            break 'run;
                        }
                    }
                }
                outcome
            } else {
                batch_id += 1;
                // Flat list for the agent, with a map back to (task, item).
                let mut tasks = Vec::new();
                let mut origin = Vec::new();
                for (bi, (index, items)) in batch.iter().enumerate() {
                    let task = &c.steps[*index].task;
                    // Per step, never per batch: a task's own `vars:` may name an interpreter,
                    // and a batch holds whatever shares a connection, not whatever shares an
                    // interpreter. Reading the batch's first step for all of them would run one
                    // task's module under another task's Python.
                    let chosen = payloads.get(index).map(|(_, asked)| {
                        chosen_interpreter(asked.as_deref(), link.interpreters())
                    });
                    let python = python_for(
                        &task.module,
                        payloads.get(index).map(|(module, _)| &**module),
                        chosen.as_ref(),
                    );
                    // A step that cannot travel - no interpreter for its payload, or no payload
                    // for a module that needs one - reports that per item, from inside
                    // `step_tasks`, and the rest of the batch goes out as it would have.
                    for (ii, built) in step_tasks(task, items, &python, &mut received[bi]) {
                        tasks.push(built);
                        origin.push((bi, ii));
                    }
                }
                let (flat, ended) = run_agent_batch(
                    link,
                    &name,
                    batch_id,
                    tasks,
                    plan.python.as_deref(),
                    &[],
                    &mut driver.stop,
                    &mut driver.stop_broken,
                    &mut logs,
                )
                .await;
                for (k, result) in flat.into_iter().enumerate() {
                    if let Some(&(bi, ii)) = origin.get(k) {
                        received[bi][ii] = result;
                    }
                }
                flat_steps = origin.iter().map(|&(bi, _)| batch[bi].0).collect();
                ended
            };
            if let Some(link) = links.get_mut(&key) {
                file_timings(
                    link,
                    &options.profile,
                    &name,
                    &c,
                    &flat_steps,
                    batch[0].0,
                    &prepare_micros,
                );
            }
            // Queued under the batch's first step, in front of the results it belongs with,
            // rather than written as it arrived: the coordinator owns everything a task shows.
            for message in logs.drain(..) {
                let _ = tx
                    .send(Event::Warning {
                        host: name.clone(),
                        index: batch[0].0,
                        message,
                        censored: false,
                    })
                    .await;
            }
            // Report every task of the batch in order; tasks the agent never reached after a
            // failure are not reported at all, as in Ansible. Whether `undecided` was actually
            // reported is tracked rather than assumed, because the agent can end the batch `Ok`
            // without a result for it and advancing past a step with no `TaskDone` behind it
            // stalls every barrier behind it.
            let mut undecided_reported = false;
            for (bi, (index, items)) in batch.iter().enumerate() {
                let task = &c.steps[*index].task;
                let mut results = Vec::new();
                let mut labels = Vec::new();
                let mut reached = true;
                for (ii, item) in items.iter().enumerate() {
                    let r = match (&item.skipped, received[bi][ii].take()) {
                        (Some(s), _) => s.clone(),
                        // The retry loop has already applied `changed_when` and `failed_when`:
                        // `until` reads what they decided, so applying them twice would judge a
                        // result that is not the module's any more.
                        (None, Some(r)) if decided => r,
                        (None, Some(r)) => finish(task, item, r, &templar),
                        (None, None) => {
                            reached = false;
                            break;
                        }
                    };
                    results.push((item.element.clone(), r));
                    labels.push(item.label.clone());
                }
                if !reached && results.is_empty() {
                    break;
                }
                if let Some(reason) = unresolved_notify(&c, task, &results) {
                    options.abort.raise(reason);
                    break 'run;
                }
                let warnings = take_warnings(&mut results);
                for message in &warnings {
                    let _ = tx
                        .send(Event::Warning {
                            host: name.clone(),
                            index: *index,
                            message: message.clone(),
                            censored: task.censors(),
                        })
                        .await;
                }
                let live = driver.progress.borrow().live_hosts.clone();
                let targets = fact_targets(task, &name, &live);
                let removed = {
                    // One lock for both: a reader between them would see a host that had
                    // registered a result without holding the facts that came in it.
                    let mut vars = store.lock().expect("vars lock");
                    record_registered(&mut vars, task, &targets, &results, &warnings);
                    // Only here, and never on the `run_local` path above: this is where a
                    // managed host's own words arrive, and `set_fact` writes its own facts with
                    // the trust each of them earned.
                    record_facts(&mut vars, &targets, &results)
                };
                // A name, never a value, so there is nothing in it for `no_log` to hide.
                for key in removed {
                    let _ = tx
                        .send(Event::Warning {
                            host: name.clone(),
                            index: *index,
                            message: format!("Removed restricted key from module data: {key}"),
                            censored: false,
                        })
                        .await;
                }
                let rescuable = !handlers_only && rescue_target(&c, *index).is_some();
                let retried: &[Vec<u32>] = if bi == 0 { &lefts } else { &[] };
                let retried_names: &[String] = if bi == 0 { &names } else { &[] };
                if let Some(result) = report_task(
                    &tx,
                    &name,
                    *index,
                    task,
                    &results,
                    &labels,
                    &items.iter().map(|i| i.ignore_errors).collect::<Vec<_>>(),
                    retried,
                    retried_names,
                    Dump::No,
                    rescuable,
                    batch_delegate.as_deref(),
                )
                .await
                {
                    failed |= !rescuable;
                    failed_at = Some((*index, result));
                    break;
                }
                notify(&c, task, &results, &mut notified);
                if undecided == Some(*index) {
                    undecided_reported = true;
                }
            }
            // The results are in and reported, so the next host may start. Only the permit goes
            // back; the links stay, for the reason the release at the top of the step loop
            // gives. The three ways out of here each close the escalated ones on their own: a
            // failure through the arm at the top of this loop, the end of the play through
            // `keep_links`.
            let carries_on =
                failed_at.is_none() && deferred_error.is_none() && undecided.is_none() && pos < n;
            if !carries_on {
                permit = None;
            }
            match ended {
                Err(msg) => {
                    unreachable = Some(msg);
                    break 'run;
                }
                Ok(BatchOutcome::Cancelled { .. }) => break 'run,
                Ok(_) => {}
            }
            // The step held back above did not fail, so it steps over its block's rescue after
            // all, and only now is that true enough to tell the coordinator. Guarded by
            // `undecided_reported`: a step this loop never actually reported has not told us
            // that, whatever `ended` says about the rest of the batch.
            if failed_at.is_none()
                && undecided_reported
                && let Some(step) = undecided
            {
                let Some(next) = driver.advance(step).await else {
                    break 'run;
                };
                pos = next;
            }
        }

        if failed_at.is_none()
            && let Some((index, err)) = deferred_error
        {
            let task = &c.steps[index].task;
            // The reference's own prefix on the `msg` of a task that dies before it runs - a
            // `when` it cannot evaluate, arguments it cannot render. Measured: a failing `when`
            // reports `Task failed: Error while evaluating conditional: ...`, so a playbook
            // testing `'Task failed' in result.msg` must still see it. `failed_when` does not get
            // it: there the reference leaves `msg` empty and fills `failed_when_result`.
            let results = vec![(
                None,
                TaskResult::failed_with(format!("Task failed: {}", err.0)),
            )];
            // Measured on ansible-core 2.19.12: an undefined variable in a task's arguments
            // fails that task, and a rescue around it takes the failure with the error's own
            // sentence in `ansible_failed_result.msg`.
            let rescuable = !handlers_only && rescue_target(&c, index).is_some();
            if let Some(result) = report_task(
                &tx,
                &name,
                index,
                task,
                &results,
                &[None],
                &[],
                &[],
                &[],
                Dump::No,
                rescuable,
                None,
            )
            .await
            {
                failed |= !rescuable;
                failed_at = Some((index, result));
            } else {
                let Some(next) = driver.advance(index).await else {
                    break 'run;
                };
                pos = next;
            }
        }
    }
    drop(permit);
    if let Some(msg) = unreachable {
        // Whatever went wrong, none of these connections is one to hand to the next play.
        for (_, link) in links.drain() {
            link.shutdown().await;
        }
        failed = true;
        let _ = tx
            .send(Event::Unreachable {
                host: name.clone(),
                msg,
                censored: unreachable_censored,
                delegate: unreachable_delegate,
            })
            .await;
    }
    // A link this driver opened to somebody else's host - a `delegate_to` - goes no further than
    // this play, because the run keeps exactly one link per host and a second one handed back for
    // a host whose own driver also handed one back would race for the same key. Awaited rather
    // than spawned: at the end of the last play the runtime can be dropped before a detached task
    // runs, leaving the delegate's agent waiting on a connection nobody closes.
    let mut closing = tokio::task::JoinSet::new();
    for key in links
        .keys()
        .filter(|k| k.host != name)
        .cloned()
        .collect::<Vec<_>>()
    {
        if let Some(link) = links.remove(&key) {
            closing.spawn(link.shutdown());
        }
    }
    while closing.join_next().await.is_some() {}
    // The healthy connection to the host itself outlives the play; `keep_links` decides which
    // of these that is and closes the rest. The run closes what is left once, before the recap.
    let _ = tx
        .send(Event::Finished {
            host: name,
            failed,
            silent_failure,
            links: links.into_iter().collect(),
        })
        .await;
}

/// Files what `link` measured since it was last asked: its blob and wire time, and one record
/// per result under the step it belongs to. `flat_steps` maps a flat batch's positions to their
/// steps; the plugin and retry paths send the one step `first`, and leave it empty.
///
/// Called on the two ways a stopped run leaves a batch as well as at its end, so a profile
/// written after an interruption still lists the attempts the run already showed.
fn file_timings(
    link: &mut AgentLink,
    profile: &Profile,
    host: &str,
    steps: &Compiled,
    flat_steps: &[usize],
    first: usize,
    prepare_micros: &HashMap<usize, u64>,
) {
    let ledger = link.take_ledger();
    profile.phase(Some(host), Phase::Blob, ledger.blob_micros);
    profile.phase(Some(host), Phase::Wire, ledger.wire_micros);
    for (k, module, ran) in ledger.ran {
        let index = flat_steps.get(k).copied().unwrap_or(first);
        profile.task(TaskRecord {
            host: host.to_string(),
            index,
            task: steps.steps[index].task.name.clone(),
            module,
            ran,
            prepare_micros: prepare_micros.get(&index).copied().unwrap_or(0),
        });
    }
}

/// The one step of a batch an action plugin backs, or the controller error that ends the host
/// when the batch holds anything else: the plugin branch runs one step, so a second one would be
/// reported by nobody and never run.
fn plugin_step<T>(batch: &[(usize, T)]) -> Result<&(usize, T), String> {
    match batch {
        [only] => Ok(only),
        _ => Err(format!(
            "a task backed by an action plugin has to be alone in its batch, and this one holds steps {:?}",
            batch.iter().map(|(index, _)| index).collect::<Vec<_>>()
        )),
    }
}

/// What a batch whose one task an action plugin backs needs to run it.
struct PluginBatch {
    kind: Kind,
    /// The interpreter the host the module runs on asked for, read off its variables.
    asked: Option<String>,
    /// That host's name and variables, when it is a delegate.
    delegate: Option<(String, HostVars)>,
}

/// What became of a `run_once` step, from the point of view of a host that did not run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOnce {
    /// The one runner finished the step without failing; whatever it registered or set as a
    /// fact is in the store for every host of the batch.
    Done,
    /// It failed. Measured on ansible-core 2.19.12: every other host leaves the play here, with
    /// no line of its own and no recap entry, and the run exits 2 - with a `rescue` around the
    /// task as well, which keeps the runner itself in the play.
    Failed,
    /// It left the play without a verdict, which is what an unreachable host does. Measured:
    /// exit 4 and no recap line for the hosts that were waiting, so their departure adds
    /// nothing of its own.
    Gone,
}

/// Everything one host's driver carries from one step to the next: who it is, what it is
/// walking, and the watches every wait below opens on.
///
/// What a wait needs of this host is the same at every step, so it lives here rather than
/// travelling through each of them. Only `cleanup` moves underneath it.
struct Driver<'a> {
    /// Where every word this host owes the coordinator, and every line it shows, goes.
    tx: &'a mpsc::Sender<Event>,
    name: &'a str,
    plan: &'a PlayPlan,
    /// The batch's shared progress, written by the coordinator alone.
    progress: watch::Receiver<Progress>,
    stop: watch::Receiver<bool>,
    /// Set once the stop watch's sender is gone, so a dropped sender is never read as an
    /// interrupt and the selects below stop polling a branch that would otherwise resolve
    /// immediately forever.
    stop_broken: bool,
    /// The index the `always` section this host is draining ends at, when it is draining one.
    /// The one index it carries that sits past a splice point, so the one a splice moves.
    cleanup: Option<usize>,
}

impl Driver<'_> {
    /// Whether the run has been interrupted.
    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    /// Tells the coordinator this host is done with the step at `index`.
    async fn task_done(&self, index: usize) {
        let _ = self
            .tx
            .send(Event::TaskDone {
                host: self.name.to_string(),
                index,
            })
            .await;
    }

    /// Asks for the banner of the step at `index`.
    async fn banner(&self, index: usize) {
        let _ = self
            .tx
            .send(Event::Banner {
                host: self.name.to_string(),
                index,
            })
            .await;
    }

    /// Waits `delay`, or gives up when the run is interrupted.
    async fn sleep_between(&mut self, delay: Duration) -> Option<()> {
        wait_or_stop(delay, &mut self.stop, &mut self.stop_broken).await
    }

    /// Waits until every other live host of the batch has finished the step in front of `pos`.
    ///
    /// This is what a step the batch meets in front of opens on, and its exit condition is its
    /// own: alone in the batch there is nobody left to wait for, which is also how the wait ends
    /// when every other host has died.
    ///
    /// `None` when the run was interrupted.
    async fn wait_for_barrier(&mut self, pos: usize) -> Option<()> {
        loop {
            if self.stopped() {
                return None;
            }
            let p = self.progress.borrow().clone();
            if p.live_hosts.len() <= 1 || p.completed_through.is_some_and(|c| c + 1 >= pos) {
                return Some(());
            }
            tokio::select! {
                changed = self.progress.changed() => {
                    // The coordinator is gone, so no further progress can be published
                    // and waiting on it would never end.
                    if changed.is_err() {
                        return Some(());
                    }
                }
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Waits for the one runner of the `run_once` step at `pos` to say what it made of it.
    ///
    /// `None` when the run was interrupted, the way every other wait in this file answers it.
    ///
    /// The verdict is read before the live host list, and that order is the whole of it: the
    /// coordinator publishes a failed result and the shrunken list in the same `Progress`, so a
    /// host that asked "is the runner still live?" first would see a runner that is merely gone
    /// and leave at exit 4 where the reference exits 2.
    async fn wait_for_run_once(&mut self, runner: &str, pos: usize) -> Option<RunOnce> {
        loop {
            if self.stopped() {
                return None;
            }
            let p = self.progress.borrow().clone();
            match p.run_once.get(&pos) {
                Some(true) => return Some(RunOnce::Failed),
                Some(false) => return Some(RunOnce::Done),
                None => {}
            }
            // The host this step was handed to has left the batch without a verdict, which is
            // what an unreachable host does. Asked of that one host and not of the list's
            // length: with three hosts waiting on a runner that died, a test for "anybody else
            // still live" would be true for ever and this wait would never end.
            if !p.live_hosts.iter().any(|h| h == runner) {
                return Some(RunOnce::Gone);
            }
            tokio::select! {
                changed = self.progress.changed() => {
                    // The coordinator is gone, so no verdict can ever be published.
                    if changed.is_err() {
                        return Some(RunOnce::Gone);
                    }
                }
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Whether `advance` from `pos` would walk past a splice point - a flush or an include - and
    /// so block waiting for the splice.
    ///
    /// A driver may only wait for a splice once it owes the coordinator nothing in front of it:
    /// the coordinator walks the step list in order and holds at each index until every host is
    /// done with it or gone, so a host waiting at a splice point while a step behind it is still
    /// unreported waits for an index the coordinator can never reach. The batch collection loop
    /// asks this before moving on from a step it has queued and not yet run.
    fn steps_over_a_splice_point(&self, compiled: &Compiled, pos: usize) -> bool {
        let next = match self.cleanup {
            Some(end) => match after_pending(compiled, pos, end) {
                Some((next, _)) => next,
                None => return false,
            },
            None => after(compiled, pos),
        };
        (pos + 1..next.min(compiled.steps.len()))
            .any(|i| crate::compile::is_splice_point(&compiled.steps[i].kind))
    }

    /// The step this host moves to after finishing `pos`, past the sections it has no reason to
    /// enter. A host draining an `always` section after a failure finishes that section instead,
    /// and `cleanup` - the index that section ends at - moves with it to the next one.
    ///
    /// Returns the length of the step list when the play is over for this host, which is what the
    /// driver's own bound reads as "done", and `None` when the run was interrupted on the way.
    async fn advance(&mut self, pos: usize) -> Option<usize> {
        let compiled = self.plan.steps();
        let next = match self.cleanup {
            Some(end) => after_pending(&compiled, pos, end).map(|(next, end)| {
                self.cleanup = Some(end);
                next
            }),
            None => Some(after(&compiled, pos)),
        };
        let Some(next) = next else {
            return Some(compiled.steps.len());
        };
        // Every flush point between here and there grows the list behind it, and every index past
        // it - this destination, and the end of the section this host may be draining - moves by
        // however much it grew.
        let grown = self.stepped_over(pos + 1..next).await?;
        if let Some(end) = self.cleanup.as_mut() {
            *end += grown;
        }
        Some(next + grown)
    }

    /// Tells the coordinator about every step of `range` this host stepped over, stopping at each
    /// splice point on the way, and answers how much longer the list is for it.
    ///
    /// A host has to stop at a splice point it steps over as surely as at one it runs. The
    /// coordinator inserts steps behind a flush or an include once every host has reported that
    /// index, and a host that read the list again before that would be holding an index into a
    /// list that has changed underneath it - the exact shape this file's barrier invariant exists
    /// to prevent. Measured on ansible-core 2.19.12: a `meta: flush_handlers` written inside a
    /// `rescue:` nobody entered runs nothing and shows nothing, and so does an `include_tasks`
    /// written there, which is what puts a live host in this position.
    async fn stepped_over(&mut self, range: std::ops::Range<usize>) -> Option<usize> {
        let mut grown = 0;
        let mut from = range.start;
        loop {
            let compiled = self.plan.steps();
            let end = range.end + grown;
            let splice = (from..end.min(compiled.steps.len()))
                .find(|&i| crate::compile::is_splice_point(&compiled.steps[i].kind));
            let Some(at) = splice else {
                self.skipped(from..end).await;
                return Some(grown);
            };
            self.skipped(from..at + 1).await;
            let before = compiled.steps.len();
            drop(compiled);
            // The bare wait, and not the one that moves `cleanup`: the growth of this splice is
            // part of what this function answers, and the caller moves `cleanup` by the whole of
            // that answer. Moving it here as well would move it twice and end the host's drain
            // past the section it is draining.
            self.wait_past_splice(at).await?;
            grown += self.plan.steps().steps.len() - before;
            // Back at the splice point's successor, which is now the first of the steps it just
            // grew by. This host steps over those too - they sit in the section the flush or the
            // include sat in, and it is not in that section - but it still owes the coordinator a
            // word about each of them, or the barrier behind them opens on a host that never said
            // it had passed them.
            from = at + 1;
        }
    }

    /// Waits until the coordinator has put the handler steps in behind the flush point at `at`,
    /// and moves the end of the `always` section this host may be draining by however much the
    /// splice grew the list - the one index it carries that sits past `at`.
    ///
    /// `before` is how long the list was when the host still owed `at` a `TaskDone`, and the
    /// caller has to read it before sending that word. Reading it here would be a race: the
    /// coordinator publishes the splice as soon as the last host reports the index, so the list
    /// can already have grown by the time this runs, and the growth would then measure as nothing
    /// at all - leaving `cleanup` where it was and ending the host's drain one step early. Rare,
    /// load dependent, and it reports success while a cleanup step nobody ran goes missing.
    ///
    /// `None` only when the run was interrupted. A coordinator that is gone answers `Some(())`:
    /// no splice will ever land, so `cleanup` stays where it is and the host carries on with the
    /// list it has, which is what `wait_past_splice` answers `false` for.
    async fn wait_for_splice(&mut self, at: usize, before: usize) -> Option<()> {
        if self.wait_past_splice(at).await?
            && let Some(end) = self.cleanup.as_mut()
        {
            *end += self.plan.steps().steps.len() - before;
        }
        Some(())
    }

    /// The wait alone, for the caller whose `cleanup` must not move here.
    ///
    /// This is the whole of the plan's one dynamic primitive on the driver's side: between
    /// reporting a splice point and this returning, the host reads nothing from the step list, so
    /// the index it holds cannot be invalidated by the splice.
    ///
    /// `true` when the splice landed, `false` when the coordinator is gone and none ever will
    /// land - which is why this is answered rather than assumed: an index that moves with the
    /// list must not move for a splice that was never published. `None` when the run was
    /// interrupted.
    async fn wait_past_splice(&mut self, at: usize) -> Option<bool> {
        loop {
            if self.stopped() {
                return None;
            }
            if self
                .progress
                .borrow()
                .spliced_through
                .is_some_and(|s| s >= at)
            {
                return Some(true);
            }
            tokio::select! {
                changed = self.progress.changed() => {
                    // The coordinator is gone, so no splice can be published and waiting on one
                    // would never end. Whatever the host has left to do, it does on the list it
                    // has.
                    if changed.is_err() {
                        return Some(false);
                    }
                }
                res = self.stop.changed(), if !self.stop_broken => {
                    if res.is_err() {
                        self.stop_broken = true;
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    /// Tells the coordinator about every step in `range` that this host stepped over, so the
    /// shared progress it publishes counts this host as having reached them.
    ///
    /// It is what keeps a host that steps over a section from waiting on itself: the progress
    /// every barrier reads is the lowest step any live host has finished, so a host that jumped
    /// from 3 to 9 without saying so would hold that figure at 3 while waiting for it to reach 8.
    /// Both halves of that are live now: a host that failed steps over the rest of the body on
    /// its way to a cleanup, and a host that failed nothing steps over every `rescue` it walks
    /// past - the second with the whole play still waiting on it.
    async fn skipped(&self, range: std::ops::Range<usize>) {
        for index in range {
            self.task_done(index).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::task;
    use super::*;

    /// The ledger is filed at the end of a batch and on both ways a stopped run leaves one, the
    /// plugin loop and the retry loop, so an interrupted run's profile keeps the attempts the
    /// run already showed. Read off this file: the driver has no harness short of a real agent.
    ///
    /// What would make this red: a `break 'run` on a stop that leaves the ledger behind.
    #[test]
    fn every_way_out_of_a_batch_files_its_timings() {
        let source = include_str!("driver.rs");
        let code = &source[..source.find("#[cfg(test)]").expect("a test module")];
        assert_eq!(
            code.matches("file_timings(").count(),
            4,
            "one definition, three calls"
        );
        for stop in [
            "if stopped {",
            "if driver.sleep_between(retry.delay).await.is_none() {",
        ] {
            let after = code.rfind(stop).expect(stop) + stop.len();
            let arm = &code[after..after + code[after..].find("break 'run;").expect("a break")];
            assert!(
                arm.contains("file_timings("),
                "{stop} leaves without filing:{arm}"
            );
        }
    }

    /// A plugin batch is exactly one step, and anything else ends the host naming the steps.
    ///
    /// What would make this red: the branch reading `batch[0]` again, which runs the first step
    /// and leaves the others unreported - a task that finishes `ok` having never run.
    #[test]
    fn a_plugin_batch_holds_its_one_step_and_nothing_else() {
        assert_eq!(plugin_step(&[(4, ())]), Ok(&(4, ())));
        let err = plugin_step(&[(4, ()), (5, ())]).expect_err("two steps");
        assert!(err.contains("[4, 5]"), "{err}");
        plugin_step::<()>(&[]).expect_err("no step");
    }
    use serde_json::json;

    /// Everything a playbook can hide a cross-host read in has to be searched, or a barrier
    /// silently does nothing and the ordering it promised was never there.
    #[test]
    fn a_cross_host_read_is_found_wherever_the_task_spells_it() {
        let mut t = task("command");
        assert!(!reads_across_hosts(&t));
        t.args
            .insert("cmd".into(), json!("echo {{ hostvars['a'].x }}"));
        assert!(reads_across_hosts(&t), "arguments");
        let mut t = task("debug");
        t.when = vec!["inventory_hostname in ansible_play_hosts".into()];
        assert!(reads_across_hosts(&t), "when");
        let mut t = task("debug");
        t.name = "count {{ ansible_play_batch | length }}".into();
        assert!(reads_across_hosts(&t), "name");
        let mut t = task("debug");
        t.vars.insert("peer".into(), json!("{{ hostvars['a'].y }}"));
        assert!(reads_across_hosts(&t), "vars");
        let mut t = task("command");
        t.loop_items = Some(json!("{{ ansible_play_hosts }}"));
        assert!(reads_across_hosts(&t), "loop");
        let mut t = task("command");
        t.changed_when = vec!["hostvars['a'].rc == 0".into()];
        assert!(reads_across_hosts(&t), "changed_when");
        let mut t = task("command");
        t.failed_when = vec!["play_hosts | length > 1".into()];
        assert!(reads_across_hosts(&t), "failed_when");
        let mut t = task("command");
        t.until = vec!["hostvars['a'].ready".into()];
        assert!(reads_across_hosts(&t), "until");
        let mut t = task("command");
        t.delegate_to = Some("{{ hostvars['a'].peer }}".into());
        assert!(reads_across_hosts(&t), "delegate_to");
        let mut t = task("command");
        t.environment = vec![json!({"PEER": "{{ hostvars['a'].ip }}"})];
        assert!(reads_across_hosts(&t), "environment");
        let mut t = task("command");
        t.ignore_errors = Some(crate::playbook::Flag::Template(
            "{{ hostvars[groups.primary[0]].degraded | default(false) }}".into(),
        ));
        assert!(reads_across_hosts(&t), "a templated ignore_errors");
    }

    /// `ansible_play_hosts_all` never changes once the play starts, so a task mentioning it and
    /// nothing else needs no barrier. The live list and the batch keyword still hold one.
    #[test]
    fn ansible_play_hosts_all_alone_is_not_a_boundary() {
        let mut t = task("debug");
        t.args.insert(
            "msg".into(),
            json!("{{ ansible_play_hosts_all | join(',') }}"),
        );
        assert!(!reads_across_hosts(&t), "the static list on its own");
        let mut t = task("debug");
        t.args.insert(
            "msg".into(),
            json!(
                "{{ ansible_play_hosts | join(',') }} of {{ ansible_play_hosts_all | join(',') }}"
            ),
        );
        assert!(reads_across_hosts(&t), "the live list is still present");
        let mut t = task("debug");
        t.args
            .insert("msg".into(), json!("{{ ansible_play_batch }}"));
        assert!(reads_across_hosts(&t), "play_batch is unaffected");
    }

    /// The step loop's boundary test is the union of two independent rules, and neither one
    /// covers the other: `run_once` is a boundary because the table says so and spells none of
    /// the cross-host names, a `hostvars` read is a boundary because of its text and carries no
    /// keyword, and `delegate_to` is neither - the delegating driver opens its own link to the
    /// delegate, so nothing is shared and nobody has to wait.
    ///
    /// This says nothing about who runs a `run_once` step. The election is the coordinator's,
    /// and `run_once_is_decided_once_and_survives_the_runner_leaving` is what guards it; the
    /// barrier here only keeps the hosts walking the list together.
    ///
    /// What would make this red: the `barrier` flag dropped from the table, which lets a
    /// `run_once` step be reported by a host while another is still behind it, so the verdict
    /// the waiting hosts read arrives before the runner has run; or the textual scan dropped,
    /// which lets a `hostvars` read run ahead of the host it reads. That a delegated task is a
    /// boundary under `batching` all the same - it acts on a host that may not have got there
    /// yet - is `mark_boundaries`' business, not the keyword table's.
    #[test]
    fn a_boundary_is_either_declared_by_a_keyword_or_found_in_the_text() {
        let mut t = task("command");
        assert!(!t.barrier() && !reads_across_hosts(&t));

        t.run_once = Some(true);
        assert!(t.barrier(), "the table declares run_once a barrier");
        assert!(
            !reads_across_hosts(&t),
            "and it spells none of the cross-host names, so the scan alone would miss it"
        );
        t.run_once = Some(false);
        assert!(!t.barrier(), "a task that says false is not one");
        t.run_once = None;

        t.delegate_to = Some("h3".into());
        t.delegate_facts = Some(true);
        assert!(
            !t.barrier() && !reads_across_hosts(&t),
            "a delegated task is neither declared a barrier nor spells a cross-host name"
        );

        let mut t = task("command");
        t.args
            .insert("cmd".into(), json!("echo {{ hostvars['a'].x }}"));
        assert!(
            reads_across_hosts(&t),
            "the scan catches what no keyword spells"
        );
        assert!(!t.barrier(), "and it needs no keyword to do it");
    }

    /// Under `batching`, a host with a fork permit kept from the batch before gives it back
    /// only in front of a boundary. A `pause` that is not one is waited out with the permit in
    /// hand, so with `forks = 2` two hosts pause while the others cannot even start the task
    /// before it, and the play takes several pause lengths where the reference takes one.
    ///
    /// What would make this red: a `pause` the batching driver walks into without meeting the
    /// other hosts, permit and all.
    #[test]
    fn a_pause_is_a_boundary_under_batching() {
        let c = marked(vec![
            task("debug"),
            task("pause"),
            task("ansible.builtin.pause"),
        ]);
        assert_eq!(boundaries(&c), [false, true, true]);
    }

    /// Every task of a batch is rendered before the batch goes out, so a task that can write
    /// facts has to be the last of its batch: the next one may read them. `hostname` is the
    /// shape that bites: `lineinfile` writing `{{ ansible_fqdn }}` into `/etc/hosts` right behind
    /// it would write the old name.
    ///
    /// What would make this red: a builtin that answers with `ansible_facts` missing from
    /// `FACT_MODULES`, or a module this release cannot vouch for - a collection's - let through.
    #[test]
    fn a_task_that_writes_facts_ends_its_batch() {
        for module in [
            "setup",
            "ansible.builtin.setup",
            "ansible.legacy.hostname",
            "hostname",
            "gather_facts",
            "dnf",
            "mount_facts",
            "package_facts",
            "ansible.builtin.service_facts",
            "getent",
            "community.general.listen_ports_facts",
            "kubernetes.core.k8s",
            "my_role_library_module",
        ] {
            assert!(writes_facts(&task(module)), "{module}");
        }
        for module in ["apt", "ansible.builtin.stat", "template", "command"] {
            assert!(!writes_facts(&task(module)), "{module}");
        }
        for module in FACT_MODULES {
            assert!(is_builtin(module), "{module} is not a builtin");
        }
    }

    /// A lookup reads the controller when its task renders, so the task starts its own batch.
    ///
    /// What would make this red: a spelling of a lookup the scan misses, which renders it behind
    /// a batch in hand - before a task in front of it wrote the file it reads.
    #[test]
    fn a_lookup_is_found_wherever_the_task_calls_it() {
        let with = |key: &str, value: &str| {
            let mut t = task("lineinfile");
            t.args.insert(key.into(), json!(value));
            t
        };
        assert!(calls_lookup(&with(
            "line",
            "{{ lookup('file', '/tmp/k.pub') }}"
        )));
        assert!(calls_lookup(&with(
            "line",
            "{{ query('pipe', 'date') | first }}"
        )));
        assert!(calls_lookup(&with("line", "{{ q('env', 'HOME') }}")));
        let mut t = task("debug");
        t.when = vec!["lookup('env', 'CI') == ''".into()];
        assert!(calls_lookup(&t), "a bare condition");
        let mut t = task("debug");
        t.loop_with = Some("fileglob".into());
        assert!(calls_lookup(&t), "a with_ loop");
        assert!(!calls_lookup(&with("line", "{{ lookup_table }} lookup q")));
    }

    fn step(task: PlayTask) -> Step {
        Step {
            kind: StepKind::Task,
            task,
            block: None,
            section: crate::compile::Section::Body,
            role: None,
            origin: Arc::default(),
            include_params: None,
            hosts: None,
        }
    }

    /// A list of plain steps, marked against no variable definitions at all.
    fn marked(tasks: Vec<PlayTask>) -> Compiled {
        let mut c = Compiled {
            steps: tasks.into_iter().map(step).collect(),
            ..Compiled::default()
        };
        mark_boundaries(&mut c, &Definitions::default(), Path::new("."));
        c
    }

    /// What `is_boundary` answers for each step under `batching`.
    fn boundaries(c: &Compiled) -> Vec<bool> {
        (0..c.steps.len())
            .map(|pos| is_boundary(c, pos, false))
            .collect()
    }

    /// A scratch tree for one test, removed when it goes out of scope.
    struct Tree(std::path::PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let dir =
                std::env::temp_dir().join(format!("volant-barrier-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Tree(dir)
        }

        fn file(&self, path: &str, text: &str) -> &Self {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
            std::fs::write(path, text).expect("a file");
            self
        }

        /// `site.yml` compiled with `roles/` beside it, gathering nothing, and marked against
        /// `base`: the task names in order, each with its answer under `batching`.
        fn marked(&self, base: &Definitions) -> Vec<(String, bool)> {
            let site = self.0.join("site.yml");
            let mut pb = crate::playbook::load(&site).unwrap_or_else(|e| panic!("{e:#}"));
            pb.plays[0].gather_facts = false;
            let search = crate::roles::RoleSearch {
                paths: vec![self.0.join("roles")],
                collections: Vec::new(),
            };
            let mut c = crate::compile::compile(
                &pb.plays[0],
                &search,
                &crate::compile::TagSelection::default(),
            )
            .unwrap_or_else(|e| panic!("{e:#}"));
            mark_boundaries(&mut c, base, &self.0);
            (0..c.steps.len())
                .filter(|&pos| c.steps[pos].kind == StepKind::Task)
                .map(|pos| (c.steps[pos].task.name.clone(), is_boundary(&c, pos, false)))
                .collect()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A role with one task file, the tasks written as a YAML list under it.
    fn role(tree: &Tree, tasks: &str) {
        tree.file("site.yml", "- hosts: all\n  roles:\n    - prereq\n")
            .file("roles/prereq/tasks/main.yml", tasks);
    }

    fn named(marks: &[(String, bool)], name: &str) -> bool {
        marks
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no task {name} in {marks:?}"))
            .1
    }

    /// The shape of the k3s proof's nftables task: the task names nothing of another host, and
    /// the template it renders loops over every host of two groups and reads each one's
    /// `ansible_default_ipv4`, under an `is defined` guard. Run ahead of the other hosts' fact
    /// gathering, the guard is false for them and their rules silently go missing. The template
    /// is the proof's own, cut down.
    ///
    /// What would make this red: the body of the template left unread, which is what the scan
    /// did before - the task's own text is clean.
    #[test]
    fn a_template_body_reading_hostvars_is_a_boundary() {
        let tree = Tree::new("template");
        role(
            &tree,
            "- name: plain\n  template:\n    src: plain.j2\n    dest: /etc/plain\n\
             - name: nft\n  ansible.builtin.template:\n    src: k3s.nft.j2\n    dest: /etc/nftables.d/k3s.nft\n\
             - name: missing\n  template:\n    src: missing.j2\n    dest: /etc/missing\n\
             - name: rendered src\n  template:\n    src: \"{{ flavour }}.j2\"\n    dest: /etc/x\n\
             - name: pulls another in\n  template:\n    src: pulls.j2\n    dest: /etc/y\n",
        );
        tree.file(
            "roles/prereq/templates/k3s.nft.j2",
            "# Allow inter-node communication (server + agent nodes)\n\
             {% for host in (groups[server_group] | default([]) + groups[agent_group] | default([])) | unique %}\n\
             {% if hostvars[host].ansible_default_ipv4 is defined %}\n\
             insert rule inet filter input ip saddr {{ hostvars[host].ansible_default_ipv4.address }} accept\n\
             {% endif %}\n\
             {% endfor %}\n\
             insert rule inet filter input tcp dport {{ api_port | default(6443) }} accept\n",
        )
        .file(
            "roles/prereq/templates/plain.j2",
            "# loaded via an include in nftables.conf\nport {{ api_port | default(6443) }}\n",
        )
        .file("roles/prereq/templates/pulls.j2", "{% include 'k3s.nft.j2' %}\n");
        let marks = tree.marked(&Definitions::default());
        assert!(named(&marks, "nft"), "{marks:?}");
        assert!(!named(&marks, "plain"), "{marks:?}");
        assert!(named(&marks, "missing"), "a file the scan cannot read");
        assert!(named(&marks, "rendered src"), "a file the scan cannot name");
        assert!(
            named(&marks, "pulls another in"),
            "a file the scan does not follow"
        );
    }

    /// A variable whose definition reads another host carries that read into every task naming
    /// it, however many names deep and whichever layer defines it. The role default is the k3s
    /// inventory's `api_endpoint` in a role's `defaults/`; the inventory one is the same line
    /// where the k3s proof writes it.
    ///
    /// Each task under test sits behind a `gap` step, so the fence behind a marked step (see
    /// `a_step_behind_a_cross_host_read_waits_for_it`) never marks it for another reason.
    ///
    /// What would make this red: the definitions left unread (every task here spells no
    /// cross-host name of its own), or the reading stopped short of a fixed point, which loses
    /// `a`: it reaches `api_url` through `b` and `c`, and every definition is pooled twice (the
    /// role's own layer and the play-wide export), so a single pass over them already goes two
    /// names deep and a chain of two would not tell.
    #[test]
    fn a_variable_defined_from_hostvars_is_a_boundary() {
        let tree = Tree::new("definition");
        role(
            &tree,
            "- name: plain\n  debug:\n    msg: \"{{ plain }}\"\n\
             - name: direct\n  debug:\n    msg: \"{{ api_url }}\"\n\
             - name: gap 1\n  debug:\n    msg: gap\n\
             - name: chained\n  debug:\n    msg: \"{{ a }}\"\n\
             - name: gap 2\n  debug:\n    msg: gap\n\
             - name: bare\n  debug:\n    msg: ok\n  when: api_url is defined\n\
             - name: gap 3\n  debug:\n    msg: gap\n\
             - name: from the inventory\n  command: \"curl {{ api_endpoint }}\"\n\
             - name: gap 4\n  debug:\n    msg: gap\n\
             - name: in a template\n  template:\n    src: conf.j2\n    dest: /etc/conf\n",
        );
        tree.file(
            "roles/prereq/defaults/main.yml",
            "api_url: \"{{ hostvars[groups['server'][0]].ansible_host }}\"\n\
             a: \"{{ b }}\"\n\
             b: \"{{ c }}/v1\"\n\
             c: \"https://{{ api_url }}:6443\"\n\
             plain: \"{{ a_local_value | default('x') }}\"\n",
        )
        .file("roles/prereq/templates/conf.j2", "server: {{ a }}\n");
        let mut inventory = Definitions::default();
        inventory.add(&serde_json::from_value(json!({
            "api_endpoint": "{{ hostvars[groups['server'][0]]['ansible_host'] | default(groups['server'][0]) }}",
        })).expect("a map"));
        let marks = tree.marked(&inventory);
        assert!(named(&marks, "direct"), "{marks:?}");
        assert!(named(&marks, "chained"), "three names deep: {marks:?}");
        assert!(named(&marks, "bare"), "a bare condition: {marks:?}");
        assert!(named(&marks, "from the inventory"), "{marks:?}");
        assert!(named(&marks, "in a template"), "{marks:?}");
        assert!(!named(&marks, "plain"), "{marks:?}");
    }

    /// A name only known once it renders can be any variable, another host's included: `vars`
    /// by subscript, the `vars` lookup, and a `template` lookup whose file the scan never opens.
    /// Spelled in a task or in a definition the task reads.
    ///
    /// What would make this red: `DYNAMIC_READS` emptied, or read only in a task's own text.
    #[test]
    fn a_dynamic_vars_read_is_a_boundary() {
        let t = |msg: &str| {
            let mut t = task("debug");
            t.args.insert("msg".into(), json!(msg));
            t
        };
        let c = marked(vec![
            t("{{ vars['peer_' ~ inventory_hostname] }}"),
            t("{{ lookup('vars', 'peer') }}"),
            t("{{ lookup('ansible.builtin.template', 'peer.j2') }}"),
            t("{{ q('vars', 'peer') }}"),
        ]);
        assert_eq!(boundaries(&c), [true; 4]);

        let mut c = Compiled {
            steps: vec![step(t("{{ peer }}"))],
            ..Compiled::default()
        };
        let mut defs = Definitions::default();
        defs.add(&serde_json::from_value(json!({"peer": "{{ vars[peer_name] }}"})).expect("a map"));
        mark_boundaries(&mut c, &defs, Path::new("."));
        assert_eq!(boundaries(&c), [true], "through a definition");
    }

    /// A fact the playbook writes while the run goes - an `include_vars` whose file name renders,
    /// a `set_fact` of a raw template - is read again by every task that names it, and nothing
    /// before the run could know what it names. From the first such write, every step is a
    /// boundary. A template a managed host returned is data and never renders, so it changes
    /// nothing.
    ///
    /// What would make this red: the flag never raised by `set_fact`, raised by an untrusted
    /// write, or not read by `strict`.
    #[test]
    fn a_fact_still_holding_a_template_makes_every_step_a_boundary() {
        let store = Mutex::new(
            VarStore::new(
                &crate::inventory::Inventory::parse_ini("h1\n").expect("an inventory"),
                None,
                Path::new("."),
                Map::new(),
            )
            .expect("a store"),
        );
        assert!(!strict(true, &store));
        assert!(strict(false, &store), "batching off is strict");
        store
            .lock()
            .unwrap()
            .set_fact("h1", "plain", json!({"a": ["x"]}));
        store
            .lock()
            .unwrap()
            .set_untrusted_fact("h1", "from_host", json!("{{ hostvars }}"));
        assert!(!strict(true, &store), "neither of these renders again");
        store
            .lock()
            .unwrap()
            .set_fact("h1", "loaded", json!({"url": ["{{ api_url }}"]}));
        assert!(strict(true, &store));
    }

    /// Tasks that read nothing of another host run ahead: the whole point of `batching`. A task
    /// named after `vars` without reading any, a template whose file reads only this host, a
    /// `set_fact` of this host's own values.
    ///
    /// What would make this red: any rule above made so wide that the proofs' ordinary tasks all
    /// wait again, which is the strict `linear` under another name.
    #[test]
    fn a_host_local_task_is_not_a_boundary() {
        let tree = Tree::new("local");
        role(
            &tree,
            "- name: Include OS-specific vars\n  include_vars: Debian.yml\n\
             - name: Install {{ package }}\n  apt:\n    name: \"{{ package }}\"\n\
             - name: conf\n  template:\n    src: local.j2\n    dest: /etc/local\n\
             - name: remember\n  set_fact:\n    seen: \"{{ ansible_facts.hostname }}:{{ port }}\"\n\
             - name: all hosts\n  debug:\n    msg: \"{{ ansible_play_hosts_all | length }}\"\n",
        );
        tree.file("roles/prereq/vars/main.yml", "port: 80\n")
            .file("roles/prereq/vars/Debian.yml", "package: nginx\n")
            .file(
                "roles/prereq/templates/local.j2",
                "listen {{ port }} on {{ inventory_hostname }}\n",
            );
        let marks = tree.marked(&Definitions::default());
        assert!(marks.iter().all(|(_, b)| !b), "{marks:?}");
        assert_eq!(marks.len(), 5, "{marks:?}");
    }

    /// `delegate_facts` writes into the delegate's variables, which the delegate reads as its
    /// own under a name no scan ties to the writer. The writer waits for everybody to be done
    /// with what came before (it is a delegated task, which is a boundary on its own), and
    /// nothing behind it may run before it.
    ///
    /// What would make this red: the rule dropped, which lets the delegate read its own `peer`
    /// two steps later before the writer wrote it.
    #[test]
    fn a_task_writing_facts_onto_another_host_holds_every_step_behind_it() {
        let mut writer = task("set_fact");
        writer.args.insert("peer".into(), json!("up"));
        writer.delegate_to = Some("h2".into());
        writer.delegate_facts = Some(true);
        let mut reader = task("debug");
        reader.args.insert("msg".into(), json!("{{ peer }}"));
        let c = marked(vec![
            task("debug"),
            writer.clone(),
            task("debug"),
            reader.clone(),
        ]);
        assert_eq!(boundaries(&c), [false, true, true, true]);
        writer.delegate_facts = Some(false);
        let c = marked(vec![task("debug"), writer, task("debug"), reader]);
        assert_eq!(boundaries(&c), [false, true, true, false]);
    }

    /// The barrier holds on both sides of a read. In front of the reader, every host is done
    /// with the step before; behind it, no host runs on until every host is done with the read -
    /// otherwise `n1` runs `set_fact: state=active` before `n2` has rendered the `extract` over
    /// `n1`'s `state`, and `n2` reads a value from the future. The strict `linear` fences both
    /// sides, and so does this, on every way out of the reader: the next step, the step past a
    /// rescue when it succeeds, the rescue when it fails.
    ///
    /// What would make this red: the fence behind a marked step dropped.
    #[test]
    fn a_step_behind_a_cross_host_read_waits_for_it() {
        let mut standby = task("set_fact");
        standby.args.insert("state".into(), json!("standby"));
        let mut reader = task("debug");
        reader.args.insert(
            "msg".into(),
            json!("{{ groups.all | map('extract', hostvars, 'state') | list }}"),
        );
        let mut active = task("set_fact");
        active.args.insert("state".into(), json!("active"));
        let c = marked(vec![standby, reader, active, task("debug")]);
        assert_eq!(boundaries(&c), [false, true, true, false]);

        let tree = Tree::new("fence");
        role(
            &tree,
            "- block:\n\
             \x20   - name: reader\n      debug:\n        msg: \"{{ hostvars['n1'].state }}\"\n\
             \x20 rescue:\n\
             \x20   - name: rescued\n      debug:\n        msg: r\n\
             - name: past the block\n  debug:\n    msg: p\n\
             - name: further on\n  debug:\n    msg: f\n",
        );
        let marks = tree.marked(&Definitions::default());
        assert!(named(&marks, "reader"), "{marks:?}");
        assert!(named(&marks, "rescued"), "where a failure goes: {marks:?}");
        assert!(
            named(&marks, "past the block"),
            "where success goes: {marks:?}"
        );
        assert!(!named(&marks, "further on"), "{marks:?}");
    }

    /// A delegated task acts on another host, which may not have got there yet: `web` running
    /// `postgresql_user` on `db` before `db` installed PostgreSQL. It is a boundary.
    ///
    /// What would make this red: `delegate_to` left out of the marks.
    #[test]
    fn a_delegated_task_is_a_boundary() {
        let mut delegated = task("command");
        delegated.delegate_to = Some("{{ groups.db[0] }}".into());
        let c = marked(vec![task("debug"), delegated, task("debug"), task("debug")]);
        assert_eq!(boundaries(&c), [false, true, true, false]);
    }

    /// A file one host puts on the controller is read by the others from the controller: `ca`
    /// fetches its certificate, every host copies it out. A task reading a controller file
    /// behind the first task that can write one waits for it, and one in front of it does not.
    ///
    /// What would make this red: the rule dropped, which lets a host copy last run's certificate
    /// while `ca` is still fetching this one.
    #[test]
    fn a_controller_file_read_behind_a_fetch_is_a_boundary() {
        let copy = |src: &str| {
            let mut t = task("copy");
            t.args.insert("src".into(), json!(src));
            t
        };
        let mut read = task("lineinfile");
        read.args
            .insert("line".into(), json!("{{ lookup('file', 'files/ca.crt') }}"));
        let c = marked(vec![
            copy("files/motd"),
            task("fetch"),
            task("debug"),
            copy("files/ca.crt"),
            task("debug"),
            read,
        ]);
        assert_eq!(boundaries(&c), [false, false, false, true, true, true]);
        let mut local = task("command");
        local
            .vars
            .insert("ansible_connection".into(), json!("local"));
        let c = marked(vec![local, task("debug"), copy("out.txt")]);
        assert_eq!(boundaries(&c), [false, false, true]);
    }

    /// Steps a splice puts in are boundaries until the list is marked again, and the marks of
    /// the steps around them move with them. A list nobody marked is all boundaries.
    ///
    /// What would make this red: a splice that leaves `crosses` where it was, which shifts every
    /// mark past the splice point onto the wrong step.
    #[test]
    fn a_splice_keeps_the_marks_on_their_steps() {
        let mut reader = task("debug");
        reader
            .args
            .insert("msg".into(), json!("{{ hostvars['h2'].x }}"));
        let mut c = marked(vec![task("debug"), reader, task("debug"), task("debug")]);
        assert_eq!(boundaries(&c), [false, true, true, false]);
        c.splice(1, vec![step(task("debug"))]);
        assert_eq!(boundaries(&c), [false, true, true, true, false]);
        mark_boundaries(&mut c, &Definitions::default(), Path::new("."));
        assert_eq!(boundaries(&c), [false, false, true, true, false]);
        c.crosses.clear();
        assert_eq!(boundaries(&c), [true; 5]);
    }
}
