// SPDX-License-Identifier: GPL-3.0-or-later
//! The debugger prompt: what a task's `debugger` keyword, or `strategy: debug`, opens behind a
//! result.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::sync::watch;

use crate::playbook::Debugger;
use crate::render::Renderer;
use crate::stats::Outcome;

use super::Abort;

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

/// Whether a result of `kind` opens the prompt under `setting`. Without a setting, the
/// strategy `debug` and `ANSIBLE_ENABLE_TASK_DEBUGGER` open it on a failure and on an
/// unreachable host, as measured; `never` keeps it shut even then.
pub(super) fn opens(setting: Option<Debugger>, by_default: bool, kind: Kind) -> bool {
    match setting {
        None => by_default && matches!(kind, Kind::Failed | Kind::Unreachable),
        Some(Debugger::Never) => false,
        Some(Debugger::Always) => true,
        Some(Debugger::OnFailed) => kind == Kind::Failed,
        Some(Debugger::OnUnreachable) => kind == Kind::Unreachable,
        Some(Debugger::OnSkipped) => kind == Kind::Skipped,
    }
}

/// `ANSIBLE_ENABLE_TASK_DEBUGGER`, read as the reference reads a boolean setting.
// The `enable_task_debugger` key of `ansible.cfg` is not read.
pub(super) fn enabled_by_environment() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("ANSIBLE_ENABLE_TASK_DEBUGGER").is_ok_and(|v| {
            matches!(
                v.trim().to_lowercase().as_str(),
                "y" | "yes" | "on" | "1" | "true" | "t" | "1.0"
            )
        })
    })
}

/// The prompt for one result, until the operator continues or quits. `c` goes on with the
/// result as it is, `q` and the end of input end the run with code 99. A pipe is read as a
/// terminal is, which is what the reference does.
pub(super) async fn ask(
    session: &mut Session,
    out: &mut Renderer,
    abort: &Abort,
    stop: &mut watch::Receiver<bool>,
) {
    let prompt = format!("[{}] TASK: {} (debug)> ", session.host, session.title);
    loop {
        out.prompt(&prompt);
        let line = read_line(stop).await.unwrap_or_else(|| "EOF".to_string());
        match session.onecmd(line.trim_end_matches(['\r', '\n']), out) {
            Next::Stay => {}
            Next::Continue => return,
            Next::Quit => return quit(out, abort),
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
