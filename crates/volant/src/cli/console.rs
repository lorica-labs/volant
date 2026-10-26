// SPDX-License-Identifier: GPL-3.0-or-later
//! The `console` command: `ansible-console` of ansible-core 2.19.12 (`cli/console.py`), a shell
//! that runs one ad hoc task per line with Volant's engine. On a terminal the line is edited with
//! history and completion; on a pipe the lines are read as they come and echo nothing.

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::PathBuf;

use clap::Parser;

use super::args::{BecomeArgs, ConnectionArgs, HostsArgs, PlaybookArgs, VaultSecretArgs};

mod builtins;
mod edit;
mod help;
mod history;
mod session;

use session::Session;

/// The arguments of `ansible-console`.
#[derive(Parser, Debug)]
#[command(disable_version_flag = true)]
pub struct ConsoleArgs {
    /// The hosts the session starts on: a host, a group or a pattern; `all` by default.
    #[arg(value_name = "pattern")]
    pub pattern: Option<String>,
    #[command(flatten)]
    pub hosts: HostsArgs,
    /// Extra variables: `key=value` pairs, inline JSON or YAML, or `@file`. Repeatable.
    #[arg(short = 'e', long = "extra-vars", value_name = "EXTRA_VARS")]
    pub extra_vars: Vec<String>,
    /// More output. Repeatable.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Directories searched for modules before the configured ones. Colon-separated, repeatable.
    #[arg(short = 'M', long = "module-path", value_name = "MODULE_PATH")]
    pub module_path: Vec<String>,
    /// Number of hosts a task runs on at once.
    #[arg(short = 'f', long = "forks", value_name = "FORKS")]
    pub forks: Option<usize>,
    /// Run the tasks in check mode.
    #[arg(short = 'C', long = "check")]
    pub check: bool,
    /// Show the differences the tasks make to files.
    #[arg(short = 'D', long = "diff")]
    pub diff: bool,
    /// The directory `group_vars/`, `host_vars/` and relative paths are found from.
    #[arg(long = "playbook-dir", value_name = "BASEDIR")]
    pub playbook_dir: Option<PathBuf>,
    /// Fail a task that runs longer than this many seconds; 0 disables.
    #[arg(long = "task-timeout", value_name = "TASK_TIMEOUT")]
    pub task_timeout: Option<u64>,
    /// Accepted; each line already runs one task.
    #[arg(long = "step")]
    pub step: bool,
    /// Accepted; the reference does not act on it.
    #[arg(long = "list-hosts")]
    pub list_hosts: bool,
    /// Remove the fact cache entries of the inventory hosts before the first task.
    #[arg(long = "flush-cache")]
    pub flush_cache: bool,
    #[command(flatten)]
    pub connect: ConnectionArgs,
    #[command(flatten)]
    pub escalation: BecomeArgs,
    #[command(flatten)]
    pub vault: VaultSecretArgs,
    /// Print the version and the paths the program works with, and stop.
    #[arg(long = "version")]
    pub version: bool,
}

impl ConsoleArgs {
    /// The options every command of the session runs with.
    fn playbook_args(&self) -> PlaybookArgs {
        PlaybookArgs {
            hosts: self.hosts.clone(),
            extra_vars: self.extra_vars.clone(),
            verbose: self.verbose,
            module_path: self.module_path.clone(),
            forks: self.forks,
            flush_cache: self.flush_cache,
            adhoc_pattern: Some(self.pattern.clone().unwrap_or_else(|| "all".into())),
            connect: self.connect.clone(),
            escalation: self.escalation.clone(),
            vault: self.vault.clone(),
            ..PlaybookArgs::default()
        }
    }
}

const INTRO: &str = "Welcome to the ansible console. Type help or ? to list commands.\n";

/// Runs the session and returns the exit code.
pub fn console(args: ConsoleArgs) -> i32 {
    if args.version {
        println!("{}", super::version::block("ansible-console", &[]));
        return 0;
    }
    let mut session = match Session::start(&args) {
        Ok(session) => session,
        Err(code) => return code,
    };
    let path = history::path();
    let loaded = path.as_deref().map(history::load).unwrap_or_default();
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    println!("{INTRO}");
    let mut kept = loaded;
    let code = if terminal {
        edit::run(&mut session, &mut kept, INTRO)
    } else {
        read_pipe(&mut session)
    };
    if let Some(path) = path {
        history::save(&path, &kept);
    }
    code
}

/// The commands of a pipe: the prompt written before each line is read, nothing echoed.
fn read_pipe(session: &mut Session) -> i32 {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    loop {
        print!("{}", session.prompt(false));
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let ended = !matches!(input.read_line(&mut line), Ok(n) if n > 0);
        let line = if ended {
            "EOF"
        } else {
            line.strip_suffix('\n').unwrap_or(&line)
        };
        if let Some(code) = session.onecmd(line) {
            return code;
        }
    }
}

#[cfg(test)]
mod tests;
