// SPDX-License-Identifier: GPL-3.0-or-later
//! What the session knows between two lines, and what one line does: `cmd.Cmd.onecmd` and
//! `ConsoleCLI.default` of the reference.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use super::ConsoleArgs;
use super::builtins::Done;
use crate::cli::adhoc::Adhoc;
use crate::cli::args::{Carried, PlaybookArgs};
use crate::cli::plays::Plays;
use crate::inventory::Inventory;
use crate::render::Renderer;

pub(super) struct Session {
    /// The options of the command line, which each task starts from.
    base: PlaybookArgs,
    pub(super) inventory: Inventory,
    /// The hosts `-l` leaves, or `None` without it.
    limit: Option<HashSet<String>>,
    pub(super) cwd: String,
    pub(super) remote_user: Option<String>,
    pub(super) r#become: bool,
    pub(super) become_user: Option<String>,
    pub(super) become_method: Option<String>,
    pub(super) check: bool,
    pub(super) diff: bool,
    pub(super) forks: usize,
    pub(super) timeout: u64,
    pub(super) verbosity: u8,
    pub(super) collections: Option<Vec<String>>,
    playbook_dir: Option<PathBuf>,
    carried: Arc<Mutex<Carried>>,
    /// Whether the prompt is written in colour.
    color: bool,
    /// Where the configuration looks for collections, `ansible_collections/` under each.
    collection_roots: Vec<PathBuf>,
}

impl Session {
    /// What `run` does before the loop: the `-v` lines, the passwords, the inventory, and the
    /// hosts of the pattern. `Err` is the exit code of a session that does not start.
    pub(super) fn start(args: &ConsoleArgs) -> Result<Self, i32> {
        let base = args.playbook_args();
        let mut out = Renderer::new(anstream::ColorChoice::Auto, args.verbose);
        let set = match crate::cli::setup::set_up(&base, &mut out) {
            Ok(Ok(set)) => set,
            Ok(Err(code)) => return Err(code),
            Err(err) => {
                crate::stats::Refusal::show_error(&err);
                return Err(crate::stats::error_code(&err));
            }
        };
        let passwords = crate::cli::passwords::ask_passwords(&base).map_err(|message| {
            eprintln!("[ERROR]: {message}");
            1
        })?;
        let limit =
            crate::cli::hosts::limit_names(&base, &set.inventory, &mut out).map_err(|e| {
                eprintln!("[ERROR]: {e:#}");
                1
            })?;
        let pattern = args.pattern.clone().unwrap_or_else(|| "all".into());
        let found = set.inventory.resolve(&pattern);
        for name in &found.unmatched {
            out.warning(
                &format!("Could not match supplied host pattern, ignoring: {name}"),
                false,
            );
        }
        let reachable = found
            .hosts
            .iter()
            .any(|h| limit.as_ref().is_none_or(|l| l.contains(&h.name)));
        if !reachable {
            eprintln!(
                "[ERROR]: Specified inventory, host pattern and/or --limit leaves us with no hosts to target."
            );
            return Err(1);
        }
        let color = anstream::AutoStream::new(std::io::stdout(), set.config.color_choice(false))
            .current_choice()
            != anstream::ColorChoice::Never;
        let carried = Carried {
            passwords,
            shown: out.take_shown(),
        };
        Ok(Self {
            inventory: set.inventory,
            limit,
            cwd: pattern,
            remote_user: args.connect.user.clone(),
            r#become: args.escalation.r#become,
            become_user: args.escalation.become_user.clone(),
            // `--become-method` defaults to the configured method, `--become-user` to nothing.
            become_method: Some(
                args.escalation
                    .become_method
                    .clone()
                    .unwrap_or_else(|| set.config.become_method.clone()),
            ),
            check: args.check,
            diff: args.diff,
            forks: set.forks,
            timeout: args.task_timeout.unwrap_or(0),
            verbosity: args.verbose,
            collections: None,
            playbook_dir: args.playbook_dir.clone(),
            carried: Arc::new(Mutex::new(carried)),
            color,
            collection_roots: crate::roles::RoleSearch::new(
                args.playbook_dir
                    .as_deref()
                    .unwrap_or(std::path::Path::new(".")),
                &set.config,
            )
            .collections,
            base,
        })
    }

    /// The names of the hosts `pattern` selects, in inventory order, within `-l`.
    pub(super) fn hosts_of(&self, pattern: &str) -> Vec<String> {
        self.inventory
            .resolve(pattern)
            .hosts
            .into_iter()
            .map(|h| h.name)
            .filter(|name| self.limit.as_ref().is_none_or(|l| l.contains(name)))
            .collect()
    }

    /// `set_prompt`: `user@cwd (hosts)[f:forks]`, then `# ` under become as root and `$ `
    /// otherwise; in bold white, or red for `# `, when `colored` and the terminal takes colour.
    pub(super) fn prompt(&self, colored: bool) -> String {
        let user = self.remote_user.clone().unwrap_or_else(login_name);
        let root = self.r#become && self.become_user.as_deref().is_none_or(|u| u == "root");
        let text = format!(
            "{user}@{} ({})[f:{}]{}",
            self.cwd,
            self.hosts_of(&self.cwd).len(),
            self.forks,
            if root { "# " } else { "$ " }
        );
        if colored && self.color {
            let code = if root { "0;31" } else { "1;37" };
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text
        }
    }

    /// One line, as `cmd.Cmd.onecmd` reads it. `Some` ends the session with that exit code.
    pub(super) fn onecmd(&mut self, line: &str) -> Option<i32> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let line = if let Some(rest) = line.strip_prefix('?') {
            format!("help {rest}")
        } else if let Some(rest) = line.strip_prefix('!') {
            format!("shell {rest}")
        } else {
            line.to_string()
        };
        let split = line
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(line.len());
        let (word, arg) = (&line[..split], line[split..].trim());
        if line == "EOF" || word == "exit" {
            print!("\nAnsible-console was exited.\n");
            return Some(0);
        }
        match self.builtin(word, arg) {
            Done::Handled => None,
            Done::Exit(code) => Some(code),
            Done::NotBuiltin => {
                self.default(&line, false);
                None
            }
        }
    }

    pub(super) fn colored(&self) -> bool {
        self.color
    }

    pub(super) fn collection_roots(&self) -> &[PathBuf] {
        &self.collection_roots
    }

    pub(super) fn carried(&self) -> &Arc<Mutex<Carried>> {
        &self.carried
    }

    /// `default`: a module and its arguments when the first word names one, the `shell` module
    /// with the whole line otherwise.
    pub(super) fn default(&mut self, line: &str, force_shell: bool) {
        if line.starts_with('#') {
            return;
        }
        let mut module = "shell".to_string();
        let mut module_args = line.to_string();
        let mut words = line.split_whitespace();
        if !force_shell
            && let Some(first) = words.next()
            && is_module(first)
        {
            module = first.to_string();
            module_args = words.collect::<Vec<_>>().join(" ");
        }
        let adhoc = match Adhoc::new(&self.cwd, &module, &module_args) {
            Ok(adhoc) => adhoc.in_console(self.keywords(), self.timeout, self.playbook_dir.clone()),
            Err((_, message)) => {
                eprintln!("[ERROR]: Unable to build command: {message}");
                return;
            }
        };
        let mut args = self.base.clone();
        args.verbose = self.verbosity;
        args.forks = Some(self.forks);
        args.adhoc_pattern = Some(self.cwd.clone());
        args.carried = Some(Arc::clone(&self.carried));
        crate::cli::run::drive(&args, &Plays::Adhoc(&adhoc));
        // The cache is flushed once, by the first task.
        self.base.flush_cache = false;
    }

    /// The play keywords the reference's `play_ds` carries; an unset one is left out.
    fn keywords(&self) -> Map<String, Value> {
        let mut keywords = Map::new();
        let mut put = |name: &str, value: Option<Value>| {
            if let Some(value) = value {
                keywords.insert(name.into(), value);
            }
        };
        put("remote_user", self.remote_user.clone().map(Value::from));
        put("become", Some(self.r#become.into()));
        put("become_user", self.become_user.clone().map(Value::from));
        put("become_method", self.become_method.clone().map(Value::from));
        put("check_mode", Some(self.check.into()));
        put("diff", Some(self.diff.into()));
        put("collections", self.collections.clone().map(Value::from));
        keywords
    }
}

/// Whether `name` is a module the session runs as one: a builtin by its short or full name, or
/// a module of a collection, named in full.
pub(super) fn is_module(name: &str) -> bool {
    volant_protocol::modules::is_builtin(name)
        || volant_protocol::modules::is_known(name)
        || (name.split('.').count() == 3
            && name.split('.').all(|part| !part.is_empty())
            && !name.starts_with("ansible.builtin.")
            && !name.starts_with("ansible.legacy."))
}

/// `getpass.getuser()`: `LOGNAME`, `USER`, `LNAME` or `USERNAME`, else the account's name.
pub(super) fn login_name() -> String {
    for name in ["LOGNAME", "USER", "LNAME", "USERNAME"] {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            return value;
        }
    }
    account_name().unwrap_or_default()
}

#[cfg(unix)]
fn account_name() -> Option<String> {
    // SAFETY: `getpwuid` returns a pointer into static storage or null; it is read at once.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_name.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr((*entry).pw_name)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

#[cfg(not(unix))]
fn account_name() -> Option<String> {
    None
}
