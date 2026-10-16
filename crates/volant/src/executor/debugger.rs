// SPDX-License-Identifier: GPL-3.0-or-later
//! The debugger prompt: what a task's `debugger` keyword, or `strategy: debug`, opens behind a
//! result.

use std::io::{BufRead, IsTerminal};

use tokio::sync::watch;

use crate::playbook::Debugger;
use crate::render::Renderer;
use crate::stats::Outcome;

use super::Abort;

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

/// Whether a result of `kind` opens the prompt under `setting`. `strategy: debug` is
/// `on_failed` for every task that does not say otherwise.
pub(super) fn opens(setting: Option<Debugger>, strategy_debug: bool, kind: Kind) -> bool {
    match setting.or(strategy_debug.then_some(Debugger::OnFailed)) {
        None | Some(Debugger::Never) => false,
        Some(Debugger::Always) => true,
        Some(Debugger::OnFailed) => kind == Kind::Failed,
        Some(Debugger::OnUnreachable) => kind == Kind::Unreachable,
        Some(Debugger::OnSkipped) => kind == Kind::Skipped,
    }
}

/// The prompt for `task` on `host`, until the operator continues or quits. `c` goes on with the
/// task as it ended, `q` and the end of input end the run with code 99.
///
/// Only a terminal is read. A run whose stdin is a file or a pipe has nobody to answer, so it is
/// the end of input at once rather than a read that may never return.
pub(super) async fn ask(
    host: &str,
    task: &str,
    out: &mut Renderer,
    abort: &Abort,
    stop: &mut watch::Receiver<bool>,
) {
    loop {
        out.prompt(&format!("[{host}] TASK: {task} (debug)> "));
        let Some(line) = read_line(stop).await else {
            return quit(out, abort);
        };
        match line.split_whitespace().next() {
            None => {}
            Some("c" | "continue") => return,
            Some("q" | "quit") => return quit(out, abort),
            Some(other) => out.display(&format!(
                "*** '{other}' is not supported yet: c continues and q quits"
            )),
        }
    }
}

fn quit(out: &mut Renderer, abort: &Abort) {
    out.display("User interrupted execution");
    abort.raise_with(99, "User interrupted execution".to_string());
    out.silence();
}

/// One line of the terminal, read on a thread of its own so an interruption does not wait for
/// the operator to press Enter. `None` at the end of input, on a stdin that is not a terminal,
/// and when the run is stopped.
async fn read_line(stop: &mut watch::Receiver<bool>) -> Option<String> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = std::io::stdin().lock().read_line(&mut line);
        let _ = tx.send(read.ok().filter(|n| *n > 0).map(|_| line));
    });
    tokio::select! {
        line = rx => line.ok().flatten(),
        _ = stop.wait_for(|stopped| *stopped) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::always(Some(Debugger::Always), false, Kind::Passed, true)]
    #[case::never_beats_the_strategy(Some(Debugger::Never), true, Kind::Failed, false)]
    #[case::on_failed(Some(Debugger::OnFailed), false, Kind::Failed, true)]
    #[case::on_failed_not_skipped(Some(Debugger::OnFailed), false, Kind::Skipped, false)]
    #[case::on_skipped(Some(Debugger::OnSkipped), false, Kind::Skipped, true)]
    #[case::on_unreachable(Some(Debugger::OnUnreachable), false, Kind::Unreachable, true)]
    #[case::on_unreachable_not_failed(Some(Debugger::OnUnreachable), false, Kind::Failed, false)]
    #[case::strategy_default_failed(None, true, Kind::Failed, true)]
    #[case::strategy_default_passed(None, true, Kind::Passed, false)]
    #[case::silent_without_either(None, false, Kind::Failed, false)]
    fn a_result_opens_the_prompt_as_the_keyword_says(
        #[case] setting: Option<Debugger>,
        #[case] strategy_debug: bool,
        #[case] kind: Kind,
        #[case] opens_it: bool,
    ) {
        assert_eq!(opens(setting, strategy_debug, kind), opens_it);
    }
}
