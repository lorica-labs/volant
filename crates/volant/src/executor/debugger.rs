// SPDX-License-Identifier: GPL-3.0-or-later
//! The debugger prompt: what a task's `debugger` keyword, or `strategy: debug`, opens behind a
//! result.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::{Map, Value};
use tokio::sync::{OwnedSemaphorePermit, watch};
use volant_protocol::TaskResult;

use crate::playbook::{Debugger, PlayTask, is_meta};
use crate::render::Renderer;
use crate::stats::Outcome;
use crate::template::TemplateError;

use super::Abort;
use super::driver::ThrottleTurn;
use super::prepare::{PlayPlan, Prepared, Strategy};

mod expr;
mod session;

use session::Next;
pub(super) use session::Session;

/// How a result looks to the keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Failed,
    Skipped,
    Unreachable,
    Passed,
}

impl Kind {
    pub(super) fn of(outcome: Outcome) -> Self {
        match outcome {
            // A rescued failure failed. One `ignore_errors` swallowed did not open the prompt in
            // the reference, measured.
            Outcome::Failed | Outcome::Rescued => Self::Failed,
            Outcome::Skipped => Self::Skipped,
            Outcome::Ok | Outcome::Changed | Outcome::Ignored => Self::Passed,
        }
    }
}

/// What the operator answered at a prompt the host waits on.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Answer {
    Continue,
    Quit,
    /// Run the task again, with the arguments the prompt edited when it edited them.
    Redo(Option<Edits>),
}

/// What the operator changed in `task.args` at the prompt before `r`. Only the changed keys
/// replace a key's value: a loop's items keep their own rendering of the others, and the
/// reference hands every item the edited arguments too, measured on ansible-core 2.19.12.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Edits {
    set: Map<String, Value>,
    removed: Vec<String>,
}

impl Edits {
    /// The edits that turn `ran` into `args`, none when they are the same.
    pub(super) fn between(ran: &Map<String, Value>, args: &Map<String, Value>) -> Option<Edits> {
        let set: Map<String, Value> = args
            .iter()
            .filter(|(key, value)| ran.get(*key) != Some(value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let removed: Vec<String> = ran
            .keys()
            .filter(|key| !args.contains_key(*key))
            .cloned()
            .collect();
        (!set.is_empty() || !removed.is_empty()).then_some(Edits { set, removed })
    }
}

/// Whether a result of `kind` opens the prompt under `setting`. Without a setting, the
/// strategy `debug` and `enable_task_debugger` open it on a failure and on an unreachable
/// host, as measured; `never` keeps it shut even then. A value the reference does not know
/// opens nothing: the task fails on it before it runs.
pub(super) fn opens(setting: Option<&Debugger>, by_default: bool, kind: Kind) -> bool {
    match setting {
        None => by_default && matches!(kind, Kind::Failed | Kind::Unreachable),
        Some(Debugger::Never | Debugger::Invalid(..)) => false,
        Some(Debugger::Always) => true,
        Some(Debugger::OnFailed) => kind == Kind::Failed,
        Some(Debugger::OnUnreachable) => kind == Kind::Unreachable,
        Some(Debugger::OnSkipped) => kind == Kind::Skipped,
    }
}

/// Whether a play opens the prompt on a task that has no `debugger`: under the strategy `debug`,
/// and when `enable_task_debugger` is on in `ansible.cfg` or `ANSIBLE_ENABLE_TASK_DEBUGGER`.
pub(super) fn by_default(strategy: Strategy) -> bool {
    strategy == Strategy::Debug || crate::config::task_debugger()
}

/// Whether a result of `kind` from `task` opens the prompt. A `meta` never does: the reference
/// runs it without a worker, so none of its results reaches the debugger.
pub(super) fn opens_for(task: &PlayTask, by_default: bool, kind: Kind) -> bool {
    !is_meta(task) && opens(task.debugger.as_ref(), by_default, kind)
}

/// Whether some result of `task` could open the prompt, which ends the batch behind it: a step
/// that may run again must not have run the ones after it already.
pub(super) fn may_open(task: &PlayTask, by_default: bool) -> bool {
    [Kind::Failed, Kind::Skipped, Kind::Unreachable, Kind::Passed]
        .into_iter()
        .any(|kind| opens_for(task, by_default, kind))
}

const KEYWORD_ERROR: &str = "Task failed: Error processing keyword 'debugger': ";

/// The failure of a task whose `debugger` is a name the reference does not know, in its words.
pub(super) fn invalid(task: &PlayTask) -> Option<TemplateError> {
    let Some(Debugger::Invalid(value, _)) = &task.debugger else {
        return None;
    };
    Some(TemplateError(format!(
        "{KEYWORD_ERROR}'{value}' is not a valid value for debugger. Must be one of never, on_unreachable, on_skipped, on_failed, always"
    )))
}

/// The causes under that failure's error block: the keyword, then the sentence, both at the
/// value, as the reference chains them.
pub(super) fn causes(task: &PlayTask, message: &str) -> Option<Value> {
    let sentence = message.strip_prefix(KEYWORD_ERROR)?;
    let at = match &task.debugger {
        Some(Debugger::Invalid(_, origin)) => origin.as_ref(),
        _ => None,
    };
    Some(serde_json::json!([
        crate::render::cause("Error processing keyword 'debugger'.", at, None),
        crate::render::cause(sentence, at, None)
    ]))
}

/// What `report_task` needs to hold a result at the prompt: whether the play opens it without a
/// keyword, the fork permit the host gives back while the operator answers, so the hosts the
/// coordinator shows first can still run, and the step's `throttle` turn, given back for the
/// same reason: turns go in order, and the host the coordinator shows first may be waiting for
/// this one. No permit under `host_pinned`, whose hosts keep their fork to the end and whose
/// lines are shown as they come, so nobody waits behind the prompt for a fork.
pub(super) struct Hold<'a> {
    pub(super) by_default: bool,
    pub(super) permit: Option<&'a mut Option<OwnedSemaphorePermit>>,
    pub(super) turn: Option<&'a mut Option<ThrottleTurn>>,
    /// The handler names a `-vv` run says it saved a notification for, in front of the task's line.
    pub(super) saved: &'a [String],
}

impl<'a> Hold<'a> {
    pub(super) fn new(plan: &PlayPlan, permit: &'a mut Option<OwnedSemaphorePermit>) -> Self {
        Hold {
            by_default: by_default(plan.strategy),
            permit: (!plan.strategy.pinned()).then_some(permit),
            turn: None,
            saved: &[],
        }
    }

    pub(super) fn with_saved(mut self, saved: &'a [String]) -> Self {
        self.saved = saved;
        self
    }

    /// The same hold, for a task that holds a `throttle` turn while it reports.
    pub(super) fn with_turn(self, turn: &'a mut Option<ThrottleTurn>) -> Self {
        Hold {
            turn: Some(turn),
            ..self
        }
    }

    /// Gives the fork and the turn back, before the host waits for the operator.
    pub(super) fn release(self) {
        if let Some(permit) = self.permit {
            *permit = None;
        }
        if let Some(turn) = self.turn {
            *turn = None;
        }
    }
}

/// The key `report_task`'s answer carries when the operator asked for the task again. Never
/// shown or registered: the driver reads it before the result goes anywhere.
const REDO_KEY: &str = "_volant_redo";

/// What `report_task` answers for `r`: a result the driver reads as a redo, with the edits if
/// there are any.
pub(super) fn redo_result(edits: Option<Edits>) -> TaskResult {
    let edits = edits.map_or(
        Value::Null,
        |e| serde_json::json!({"set": e.set, "removed": e.removed}),
    );
    TaskResult(Map::from_iter([(REDO_KEY.to_string(), edits)]))
}

/// Whether `result` is a redo, and with which edits.
pub(super) fn redo_of(result: &TaskResult) -> Option<Option<Edits>> {
    let edits = result.0.get(REDO_KEY)?;
    let set = edits.get("set").and_then(Value::as_object).cloned();
    let removed = edits
        .get("removed")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let removed = removed
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    Some(set.map(|set| Edits { set, removed }))
}

/// `prepared` with the edits the prompt made before `r`, when it is the step they were made at;
/// whatever step is prepared next, they are spent. An edited value was rendered at the prompt
/// and is not rendered again, and counts as a managed host's, since the operator may have
/// copied it there from `task_vars`.
pub(super) fn with_edited(
    mut prepared: Result<Prepared, TemplateError>,
    pos: usize,
    edited: &mut Option<(usize, Edits)>,
) -> Result<Prepared, TemplateError> {
    let Some((_, edits)) = edited.take().filter(|(at, _)| *at == pos) else {
        return prepared;
    };
    if let Ok(Prepared::Local(items, _) | Prepared::Remote(items, ..)) = &mut prepared {
        for item in items {
            for key in &edits.removed {
                item.args.remove(key);
            }
            for (key, value) in &edits.set {
                item.args.insert(key.clone(), value.clone());
                item.args_untrusted.insert(key.clone());
            }
        }
    }
    prepared
}

/// The prompt for one result, until the operator continues, redoes or quits. `c` goes on with
/// the result as it is, `r` runs the task again, `q` and the end of input end the run with code
/// 99. A pipe is read as a terminal is, which is what the reference does.
pub(super) async fn ask(
    session: &mut Session,
    out: &mut Renderer,
    abort: &Abort,
    stop: &mut watch::Receiver<bool>,
) -> Answer {
    let prompt = format!("[{}] TASK: {} (debug)> ", session.host, session.title);
    loop {
        out.prompt(&prompt);
        let line = read_line(stop).await.unwrap_or_else(|| "EOF".to_string());
        match session.onecmd(line.trim_end_matches(['\r', '\n']), out) {
            Next::Stay => {}
            Next::Continue => return Answer::Continue,
            Next::Redo => return Answer::Redo(Edits::between(&session.ran, &session.args)),
            Next::Quit => {
                quit(out, abort);
                return Answer::Quit;
            }
        }
    }
}

fn quit(out: &mut Renderer, abort: &Abort) {
    out.display("User interrupted execution");
    abort.raise_with(99, "User interrupted execution".to_string());
    out.silence();
}

/// The one thread that reads standard input, a line at a time and only when asked, so a line
/// typed for one prompt is never taken by a read nobody waits for any more.
struct Reader {
    ask: std::sync::mpsc::Sender<()>,
    lines: tokio::sync::Mutex<Lines>,
}

struct Lines {
    rx: tokio::sync::mpsc::UnboundedReceiver<Option<String>>,
    /// A line was asked for and has not been taken yet: an interrupted read leaves it to the
    /// next one.
    asked: bool,
}

fn reader() -> &'static Reader {
    static READER: OnceLock<Reader> = OnceLock::new();
    READER.get_or_init(|| {
        let (ask, asked) = std::sync::mpsc::channel::<()>();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            // Ends at the end of input: nothing is read after it.
            while asked.recv().is_ok() {
                let mut line = String::new();
                let read = std::io::stdin().lock().read_line(&mut line);
                let line = read.ok().filter(|n| *n > 0).map(|_| line);
                let end = line.is_none();
                if tx.send(line).is_err() || end {
                    break;
                }
            }
        });
        Reader {
            ask,
            lines: tokio::sync::Mutex::new(Lines { rx, asked: false }),
        }
    })
}

/// One line of standard input, read on the reader thread so an interruption does not wait
/// for the operator to press Enter. `None` at the end of input and when the run is stopped.
pub(super) async fn read_line(stop: &mut watch::Receiver<bool>) -> Option<String> {
    if *stop.borrow() {
        return None;
    }
    let reader = reader();
    let mut lines = reader.lines.lock().await;
    if !lines.asked {
        reader.ask.send(()).ok()?;
        lines.asked = true;
    }
    tokio::select! {
        line = lines.rx.recv() => {
            lines.asked = false;
            line.flatten()
        }
        _ = stop.wait_for(|stopped| *stopped) => None,
    }
}

/// `--step`: the answer to "Perform task", asked once per task whichever host reaches it first.
pub(super) struct Stepper {
    /// Set by `c`: no task is asked about from then on, in this play and the ones behind it.
    carry_on: Arc<AtomicBool>,
    /// Keyed by the step's index and its task, because a splice can put another task on an
    /// index a host has already been asked about.
    answers: tokio::sync::Mutex<HashMap<(usize, String), bool>>,
    /// Set once the input ended or the run stopped: nobody is asked again, and the hosts that
    /// come after the first get the same `None` without a prompt of their own.
    closed: AtomicBool,
}

impl Stepper {
    pub(super) fn new(carry_on: Arc<AtomicBool>) -> Self {
        Stepper {
            carry_on,
            answers: tokio::sync::Mutex::default(),
            closed: AtomicBool::new(false),
        }
    }

    /// Whether the task at `index` named `task`, shown as `label`, runs. Anything but `y`, `yes`, `c` and
    /// `continue` skips it, as the reference does. `None` when the input ended or the run was
    /// stopped.
    pub(super) async fn runs(
        &self,
        index: usize,
        task: &str,
        label: &str,
        stop: &mut watch::Receiver<bool>,
    ) -> Option<bool> {
        if self.carry_on.load(Ordering::SeqCst) {
            return Some(true);
        }
        let mut answers = self.answers.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        let key = (index, task.to_string());
        if let Some(known) = answers.get(&key) {
            return Some(*known);
        }
        // `c` may have been answered while this host waited for the lock.
        if self.carry_on.load(Ordering::SeqCst) {
            return Some(true);
        }
        print!("Perform task: TASK: {label} (N)o/(y)es/(c)ontinue: ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        // Only a terminal answers `--step`: a file or a pipe is the end of input at once.
        let line = if std::io::stdin().is_terminal() {
            read_line(stop).await
        } else {
            None
        };
        let Some(line) = line else {
            self.closed.store(true, Ordering::SeqCst);
            return None;
        };
        let answer = line.trim().to_lowercase();
        if answer == "c" || answer == "continue" {
            self.carry_on.store(true, Ordering::SeqCst);
        }
        let runs = matches!(answer.as_str(), "y" | "yes" | "c" | "continue");
        answers.insert(key, runs);
        Some(runs)
    }
}

#[cfg(test)]
mod tests;
