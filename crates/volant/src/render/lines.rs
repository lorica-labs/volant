// SPDX-License-Identifier: GPL-3.0-or-later
//! The lines the reference prints whichever callback is in use, and a failure's `task path:`.

use std::io::Write;

use super::Renderer;
use super::log;
use super::style::{DEBUG, VERBOSE};

impl Renderer {
    /// A line no callback owns: it reaches stdout though a Python plugin prints the results.
    fn shown(&mut self, line: &str) {
        if self.python_stdout {
            let _ = writeln!(anstream::stdout(), "{line}");
        } else {
            let _ = writeln!(self.out, "{line}");
        }
    }

    /// The `task path:` line under a banner at `-vv`: the file made absolute, and the line the
    /// task starts on.
    pub fn task_path(&mut self, origin: &crate::playbook::Origin) {
        if self.verbosity >= 2 && self.minimal.is_none() {
            self.path_line(origin);
        }
    }

    /// The `task path:` line in front of a failed task's `fatal:` line, below `-vv` and when
    /// `show_task_path_on_failure` is on. Measured on ansible-core 2.19.12: it follows the
    /// `[ERROR]` block, and a loop item's `failed:` line, an unreachable host and a task that no
    /// file holds get none.
    pub fn failure_path(&mut self, origin: Option<&crate::playbook::Origin>) {
        if self.path_on_failure
            && self.minimal.is_none()
            && self.verbosity < 2
            && let Some(origin) = origin
        {
            self.path_line(origin);
        }
    }

    fn path_line(&mut self, origin: &crate::playbook::Origin) {
        let path = std::path::absolute(&*origin.path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&*origin.path));
        let line = format!("task path: {}:{}", path.display(), origin.line);
        let _ = writeln!(self.out, "{}", self.paint(DEBUG, &line));
    }

    /// Whether `-v` was given `level` times or more.
    pub fn verbose_at(&self, level: u8) -> bool {
        self.verbosity >= level
    }

    /// A line that shows from `level` `-v`s up, in the colour the reference gives them.
    pub fn verbose(&mut self, level: u8, line: &str) {
        if self.verbosity >= level {
            let line = self.paint(VERBOSE, line);
            self.shown(&line);
        } else if log::logs_level(level) {
            log::record(line);
        }
    }

    /// A line shown as it is: the version block, whose colour the reference leaves alone.
    pub fn plain(&mut self, level: u8, line: &str) {
        if self.verbosity >= level {
            self.shown(line);
        } else if log::logs_level(level) {
            log::record(line);
        }
    }
}
