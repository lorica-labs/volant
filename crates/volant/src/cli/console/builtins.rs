// SPDX-License-Identifier: GPL-3.0-or-later
//! The console's own commands: the `do_*` methods of `cli/console.py`, with their messages.

use super::session::Session;
use crate::cli::announce::py_str;
use crate::render::Renderer;

/// What a command did: nothing more to do, or the session ends with this exit code.
pub(super) enum Done {
    Handled,
    NotBuiltin,
    Exit(i32),
}

impl Session {
    /// Runs `word` with `arg` when it is one of the console's own commands.
    pub(super) fn builtin(&mut self, word: &str, arg: &str) -> Done {
        match word {
            "cd" => self.cd(arg),
            "list" => self.list(arg),
            "forks" | "serial" => return self.forks(arg),
            "collections" => self.collections(arg),
            "verbosity" => self.verbosity(arg),
            "become" => {
                if arg.is_empty() {
                    println!("Please specify become value, e.g. `become yes`");
                } else {
                    self.r#become = boolean(arg);
                    self.v(&format!("become changed to {}", py_bool(self.r#become)));
                }
            }
            "remote_user" => {
                if arg.is_empty() {
                    println!("Please specify a remote user, e.g. `remote_user root`");
                } else {
                    self.remote_user = Some(arg.into());
                }
            }
            "become_user" => {
                if arg.is_empty() {
                    println!("Please specify a user, e.g. `become_user jenkins`");
                    self.v(&format!(
                        "Current user is {}",
                        py_opt(self.become_user.as_deref())
                    ));
                } else {
                    self.become_user = Some(arg.into());
                }
            }
            "become_method" => {
                if arg.is_empty() {
                    println!("Please specify a become_method, e.g. `become_method su`");
                    let method = py_opt(self.become_method.as_deref());
                    self.v(&format!("Current become_method is {method}"));
                } else {
                    self.become_method = Some(arg.into());
                    self.v(&format!("become_method changed to {arg}"));
                }
            }
            "check" => {
                if arg.is_empty() {
                    println!("Please specify check mode value, e.g. `check yes`");
                    self.v(&format!("check mode is currently {}.", py_bool(self.check)));
                } else {
                    self.check = boolean(arg);
                    println!("check mode changed to {}", py_bool(self.check));
                }
            }
            "diff" => {
                if arg.is_empty() {
                    println!("Please specify a diff value , e.g. `diff yes`");
                    self.v(&format!("diff mode is currently {}", py_bool(self.diff)));
                } else {
                    self.diff = boolean(arg);
                    println!("diff mode changed to {}", py_bool(self.diff));
                }
            }
            "timeout" => self.timeout(arg),
            "shell" => self.default(arg, true),
            "help" => self.help(arg),
            _ => return Done::NotBuiltin,
        }
        Done::Handled
    }

    /// `display.v`: shown from verbosity 1 on.
    pub(super) fn v(&self, line: &str) {
        if self.verbosity >= 1 {
            println!("{line}");
        }
    }

    /// A warning through the session's renderer, so that it is written once per session.
    pub(super) fn warn(&self, text: &str) {
        let mut out = Renderer::new(anstream::ColorChoice::Auto, self.verbosity);
        let mut carried = self.carried().lock().expect("one command at a time");
        out.set_shown(std::mem::take(&mut carried.shown));
        out.warning(text, false);
        carried.shown = out.take_shown();
    }

    fn cd(&mut self, arg: &str) {
        if arg.is_empty() {
            self.cwd = "*".into();
        } else if "/*".contains(arg) {
            self.cwd = "all".into();
        } else {
            let found = self.inventory.resolve(arg);
            for name in &found.unmatched {
                self.warn(&format!(
                    "Could not match supplied host pattern, ignoring: {name}"
                ));
            }
            if self.hosts_of(arg).is_empty() {
                println!("no host matched");
            } else {
                self.cwd = arg.into();
            }
        }
    }

    fn list(&self, arg: &str) {
        match arg {
            "" => {
                for host in self.hosts_of(&self.cwd) {
                    println!("{host}");
                }
            }
            "groups" => {
                for group in self.inventory.group_names() {
                    println!("{group}");
                }
            }
            _ => {
                eprintln!("[ERROR]: Invalid option passed to \"list\"");
                println!(
                    "List the hosts in the current group or a list of groups if you add 'groups'."
                );
            }
        }
    }

    fn forks(&mut self, arg: &str) -> Done {
        if arg.is_empty() {
            println!("Usage: forks <number>");
            return Done::Handled;
        }
        match py_int(arg) {
            Err(why) => {
                // The reference catches `TypeError` where `int()` raises `ValueError`, so the
                // console ends with its unexpected-exception code.
                eprintln!("[ERROR]: Unexpected Exception, this is probably a bug: {why}");
                return Done::Exit(250);
            }
            Ok(forks) if forks > 0 => {
                self.forks = usize::try_from(forks).unwrap_or(usize::MAX);
            }
            Ok(_) => println!("forks must be greater than or equal to 1"),
        }
        Done::Handled
    }

    fn collections(&mut self, arg: &str) {
        if arg.is_empty() || arg == "none" {
            self.collections = None;
        } else {
            let list = self.collections.get_or_insert_with(Vec::new);
            list.extend(arg.split(',').map(|c| c.trim().to_string()));
        }
        match &self.collections {
            Some(list) if !list.is_empty() => {
                self.v(&format!(
                    "Collections name search is set to: {}",
                    list.join(", ")
                ));
            }
            _ => self.v("Collections name search is using defaults"),
        }
    }

    fn verbosity(&mut self, arg: &str) {
        if arg.is_empty() {
            println!("Usage: verbosity <number>");
            return;
        }
        match py_int(arg) {
            Ok(level) => {
                self.verbosity = u8::try_from(level.clamp(0, 255)).unwrap_or(0);
                self.v(&format!("verbosity level set to {arg}"));
            }
            Err(why) => eprintln!("[ERROR]: The verbosity must be a valid integer: {why}"),
        }
    }

    fn timeout(&mut self, arg: &str) {
        if arg.is_empty() {
            println!("Usage: timeout <seconds>");
            return;
        }
        match py_int(arg) {
            Ok(seconds) if seconds < 0 => eprintln!(
                "[ERROR]: The timeout must be greater than or equal to 1, use 0 to disable"
            ),
            Ok(seconds) => self.timeout = seconds.unsigned_abs(),
            Err(why) => eprintln!(
                "[ERROR]: The timeout must be a valid positive integer, or 0 to disable: {why}"
            ),
        }
    }
}

/// `int(text)`, with Python's message when it does not read as one.
fn py_int(text: &str) -> Result<i64, String> {
    text.trim()
        .replace('_', "")
        .parse::<i64>()
        .map_err(|_| format!("invalid literal for int() with base 10: {}", py_str(text)))
}

/// `boolean(value, strict=False)`.
fn boolean(text: &str) -> bool {
    ["y", "yes", "on", "1", "true", "t", "1.0"].contains(&text.to_lowercase().as_str())
}

fn py_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

fn py_opt(value: Option<&str>) -> &str {
    value.unwrap_or("None")
}
