// SPDX-License-Identifier: GPL-3.0-or-later
//! The session on a terminal: lines edited with history and Tab completion, through rustyline.
//! Tab completes the command names, after `cd` the host and group names, and after a module the
//! `option=` names Volant knows for it.

use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::MemHistory;
use rustyline::validate::Validator;
use rustyline::{Config, Context, Editor, Helper};

use super::session::Session;

/// What Tab offers, refreshed from the session before each line.
#[derive(Default)]
struct Completion {
    commands: Vec<String>,
    cd_names: Vec<String>,
}

impl Completion {
    fn refresh(&mut self, session: &Session) {
        if self.commands.is_empty() {
            self.commands = super::help::command_names(session);
        }
        // `complete_cd`: every host and group from the top, else the hosts of the current pattern.
        self.cd_names = if ["all", "*", "\\"].contains(&session.cwd.as_str()) {
            let mut names = session.hosts_of("all");
            names.extend(session.inventory.group_names());
            names
        } else {
            session.hosts_of(&session.cwd)
        };
    }
}

/// The documented options of a module this release runs, `option=` each.
fn options(module: &str) -> Vec<String> {
    use volant_protocol::modules::{local, native};
    native(module)
        .or_else(|| local(module))
        .map(|spec| {
            spec.args
                .iter()
                .filter(|a| !a.name.starts_with('_'))
                .map(|a| format!("{}=", a.name))
                .collect()
        })
        .unwrap_or_default()
}

impl Completer for Completion {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let before = &line[..pos];
        let starting = |names: &[String], word: &str| -> Vec<String> {
            names
                .iter()
                .filter(|n| n.starts_with(word))
                .cloned()
                .collect()
        };
        if let Some(rest) = before.strip_prefix("cd ") {
            return Ok((pos - rest.len(), starting(&self.cd_names, rest)));
        }
        if !before.contains(' ') {
            return Ok((0, starting(&self.commands, before)));
        }
        let start = before.rfind(' ').map_or(0, |i| i + 1);
        let module = before.split_whitespace().next().unwrap_or_default();
        Ok((start, starting(&options(module), &before[start..])))
    }
}

impl Hinter for Completion {
    type Hint = String;
}
impl Highlighter for Completion {}
impl Validator for Completion {}
impl Helper for Completion {}

/// The loop on a terminal; returns the exit code. `kept` gains each line read, for the history
/// file. Ctrl-C at the prompt starts the session's banner and prompt again, ctrl-D ends it.
pub(super) fn run(session: &mut Session, kept: &mut Vec<String>, intro: &str) -> i32 {
    let config = Config::builder()
        .auto_add_history(false)
        .bracketed_paste(false)
        .build();
    let Ok(mut editor) = Editor::<Completion, MemHistory>::with_history(config, MemHistory::new())
    else {
        return super::read_pipe(session);
    };
    for line in kept.iter() {
        let _ = editor.add_history_entry(line.as_str());
    }
    editor.set_helper(Some(Completion::default()));
    loop {
        if let Some(helper) = editor.helper_mut() {
            helper.refresh(session);
        }
        let line = match editor.readline(&session.prompt(true)) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => {
                println!("{intro}");
                continue;
            }
            Err(_) => "EOF".to_string(),
        };
        if !line.is_empty() && line != "EOF" {
            let _ = editor.add_history_entry(line.as_str());
            kept.push(line.clone());
        }
        if let Some(code) = session.onecmd(&line) {
            return code;
        }
    }
}
