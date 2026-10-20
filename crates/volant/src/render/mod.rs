// SPDX-License-Identifier: GPL-3.0-or-later
//! Console output in the shape of ansible-playbook's default callback.

use std::io::Write;

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};

use crate::stats::{CustomStats, Stats};

mod error;
mod recap_json;
mod result;

pub(crate) use error::source_context;
pub use error::{
    CHAIN_KEY, ERROR_KEY, ErrorBlock, UNKNOWN_KEY, cause, concat_message, lead, shorten,
    task_failed,
};
pub(crate) use recap_json::ansible_json;
use recap_json::dumps_indented;

/// How a task's result lines are cleaned before they are shown. Whether a body goes on the line
/// at verbosity 0 is the result's own [`VERBOSE_ALWAYS`], not the task's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dump {
    /// Shown as it is.
    No,
    /// A `debug`'s: cleaned the way the reference's callback cleans one (`_clean_results`).
    Debug,
    /// A `setup`'s or `gather_facts`': an `ok` or `changed` line carries no body at any
    /// verbosity (`_clean_results` empties it), measured at `-vv`.
    Setup,
}

/// The key a `debug` or a non-`quiet` `assert` puts on its own result, as the reference's action
/// plugins do: its body is shown at verbosity 0, indented. A failure the executor raised instead
/// (a keyword that does not convert, a timeout out of range) carries none, so it prints on one
/// line whatever the module. Never shown, never registered: every `_ansible_` key is internal.
pub const VERBOSE_ALWAYS: &str = "_ansible_verbose_always";

pub struct Renderer {
    out: Box<dyn Write>,
    color: bool,
    width: usize,
    verbosity: u8,
    /// Every `[WARNING]:` line written so far, which is never written twice.
    warned: std::collections::HashSet<String>,
    /// Every `[ERROR]:` block written so far, which is never written twice either.
    errored: std::collections::HashSet<String>,
    /// Where a result's `diff` becomes text: ansible-core's `_get_diff`, behind the run's helper.
    diffs: Option<std::sync::Arc<crate::bridge::Bridge>>,
    /// `show_task_path_on_failure`: a failure line is preceded by the task's `task path:` line
    /// when the run is below `-vv`, which already prints it under every banner.
    path_on_failure: bool,
}

/// Set by the executor on a result whose task asked for diffs; the renderer reads it and the
/// `_volant_` prefix keeps it off every line and out of every registered value.
pub const DIFF_KEY: &str = "_volant_diff";

const HOST_COLUMN: usize = 26;

/// What a `no_log` result shows instead of itself, measured on ansible-core 2.19.12 - the body
/// of every censored line is `{"censored": CENSORED, "changed": <changed>}`.
pub const CENSORED: &str =
    "the output has been hidden due to the fact that 'no_log: true' was specified for this result";

/// What a `no_log` loop item shows instead of its label, measured on the same release:
/// `changed: [h1] => (item=(censored due to no_log))`.
pub const CENSORED_ITEM: &str = "(censored due to no_log)";

const OK: Style = AnsiColor::Green.on_default();
const CHANGED: Style = AnsiColor::Yellow.on_default();
const FAILED: Style = AnsiColor::Red.on_default();
const UNREACHABLE: Style = AnsiColor::BrightRed.on_default();
const SKIPPED: Style = AnsiColor::Cyan.on_default();
const WARNING: Style = AnsiColor::BrightMagenta.on_default();
/// The reference's `dark gray`, its `COLOR_DEBUG`.
const DEBUG: Style = AnsiColor::BrightBlack.on_default();
pub(crate) const VERBOSE: Style = AnsiColor::Blue.on_default();

impl Renderer {
    /// Stdout renderer. Colour follows the terminal, `NO_COLOR`, `CLICOLOR_FORCE`, or the caller.
    pub fn new(choice: ColorChoice, verbosity: u8) -> Self {
        let stream = AutoStream::new(std::io::stdout(), choice);
        let color = stream.current_choice() != ColorChoice::Never;
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .filter(|w| *w >= 40)
            .unwrap_or(79);
        Self {
            out: Box::new(stream),
            color,
            width,
            verbosity,
            warned: std::collections::HashSet::new(),
            errored: std::collections::HashSet::new(),
            diffs: None,
            path_on_failure: false,
        }
    }

    #[cfg(test)]
    pub fn with_writer(out: Box<dyn Write>, color: bool, width: usize, verbosity: u8) -> Self {
        Self {
            out,
            color,
            width,
            verbosity,
            warned: std::collections::HashSet::new(),
            errored: std::collections::HashSet::new(),
            diffs: None,
            path_on_failure: false,
        }
    }

    /// Turns on the `task path:` line before a failure, from `show_task_path_on_failure`.
    pub fn set_path_on_failure(&mut self, on: bool) {
        self.path_on_failure = on;
    }

    /// Serves the diffs of the results this renderer prints.
    pub fn set_diffs(&mut self, bridge: std::sync::Arc<crate::bridge::Bridge>) {
        self.diffs = Some(bridge);
    }

    fn paint(&self, style: Style, text: &str) -> String {
        if self.color {
            format!("{style}{text}{style:#}")
        } else {
            text.to_string()
        }
    }

    /// The star run fills the width past the text, so the line is one character wider than the
    /// width, and never shorter than three stars: what `ansible-playbook` prints.
    fn banner(&mut self, text: &str) {
        let stars = self.width.saturating_sub(text.chars().count()).max(3);
        let _ = writeln!(self.out, "\n{text} {}", "*".repeat(stars));
    }

    pub fn play(&mut self, name: &str) {
        self.banner(&format!("PLAY [{name}]"));
    }

    /// The banner of `play`: its name, or no name at all for a play whose `hosts` rendered to
    /// `none`, which the reference prints without brackets.
    pub fn play_of(&mut self, play: &crate::playbook::Play) {
        if play.nameless {
            self.banner("PLAY");
        } else {
            self.play(&play.name);
        }
    }

    pub fn task(&mut self, name: &str) {
        self.banner(&format!("TASK [{name}]"));
    }

    /// The `task path:` line under a banner at `-vv`: the file made absolute, and the line the
    /// task starts on.
    pub fn task_path(&mut self, origin: &crate::playbook::Origin) {
        if self.verbosity >= 2 {
            self.path_line(origin);
        }
    }

    /// The `task path:` line in front of a failed task's `fatal:` line, below `-vv` and when
    /// `show_task_path_on_failure` is on. Measured on ansible-core 2.19.12: it follows the
    /// `[ERROR]` block, and a loop item's `failed:` line, an unreachable host and a task that no
    /// file holds get none.
    pub fn failure_path(&mut self, origin: Option<&crate::playbook::Origin>) {
        if self.path_on_failure
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
            let _ = writeln!(self.out, "{}", self.paint(VERBOSE, line));
        }
    }

    /// A line shown as it is: the version block, whose colour the reference leaves alone.
    pub fn plain(&mut self, level: u8, line: &str) {
        if self.verbosity >= level {
            let _ = writeln!(self.out, "{line}");
        }
    }

    /// What the reference prints when a playbook starts at `-vv`: its banner, at `-vvvv` the
    /// options it was run with, and how many plays it holds.
    pub fn playbook(&mut self, file: &std::path::Path, plays: usize, options: &[String]) {
        if self.verbosity < 2 {
            return;
        }
        let base = file.file_name().map_or_else(
            || file.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        self.banner(&format!("PLAYBOOK: {base}"));
        for line in options {
            self.verbose(4, line);
        }
        self.verbose(2, &format!("{plays} plays in {}", file.display()));
    }

    /// The banner a handler gets instead of a task's, measured on ansible-core 2.19.12:
    /// `RUNNING HANDLER [second handler]`, and `RUNNING HANDLER [base : base handler]` for one
    /// that came out of a role.
    pub fn handler(&mut self, name: &str) {
        self.banner(&format!("RUNNING HANDLER [{name}]"));
    }

    /// The line an include prints once the coordinator knows which hosts asked for what.
    ///
    /// Measured on ansible-core 2.19.12: `included: /abs/inc-a.yml for h1, h2` for a file, with
    /// the absolute path and the hosts in the play's own order; `included: base for h1` for a
    /// role, by its bare name; and `=> (item=1)` behind either when a loop asked for it. It is
    /// painted like an `ok:` line, which is what the reference's own callback does with it.
    pub fn included(&mut self, what: &str, hosts: &[String], label: Option<&str>) {
        let item = label.map(|l| format!(" => (item={l})")).unwrap_or_default();
        let line = format!("included: {what} for {}{item}", hosts.join(", "));
        let _ = writeln!(self.out, "{}", self.paint(OK, &line));
    }

    /// What the reference prints, once from the task and once from the play, when a batch fails
    /// more hosts than `max_fail_percentage` allows.
    pub fn no_more_hosts(&mut self) {
        self.banner("NO MORE HOSTS LEFT");
    }

    pub fn no_hosts(&mut self) {
        let _ = writeln!(
            self.out,
            "{}",
            self.paint(SKIPPED, "skipping: no hosts matched")
        );
    }

    /// A warning goes to stderr, which is where ansible-playbook puts every one of its own -
    /// measured on ansible-core 2.19.12, and measured here as a difference: the listing golden
    /// compares stdout alone, and a `[WARNING]` about an unmatched host pattern was landing in
    /// the middle of it. A warning mixed into stdout also reaches anything piping a listing or
    /// a JSON stream into another program, which is the reason the reference separates them.
    ///
    /// `censored` is the `no_log` of the task the warning belongs to: a diagnostic a task
    /// produced is that task speaking, so the policy that hides its result hides this too. It is
    /// false for every warning the engine writes about itself, and false for the `environment`
    /// warning as well - measured on ansible-core 2.19.12, that one quotes the playbook's own
    /// source (`['{{ secret }}']`) rather than a rendered value, so there is nothing in it to
    /// hide and the reference does not hide it either.
    ///
    /// A line already written is not written again, for the whole run, whichever task or host
    /// it came from: ansible-core 2.19.12's `Display._warning` keeps every `[WARNING]: <text>`
    /// it has shown in `_warns` and drops a repeat (`_deduplicate`, keyed by that text alone). A
    /// registered result still holds every copy, as the reference's does.
    pub fn warning(&mut self, text: &str, censored: bool) {
        let body = if censored { CENSORED } else { text };
        let line = format!("[WARNING]: {body}");
        if !self.warned.insert(line.clone()) {
            return;
        }
        let _ = writeln!(anstream::stderr(), "{}", self.paint(WARNING, &line));
    }

    /// A prompt, left open: no newline follows it, so whatever comes next is written on its line.
    pub fn prompt(&mut self, text: &str) {
        let _ = write!(self.out, "{text}");
        let _ = self.out.flush();
    }

    /// Writes nothing more to stdout. A run that ends at the debugger's `quit` leaves the
    /// prompt's line as the last one, and the hosts still being drained say nothing after it.
    pub fn silence(&mut self) {
        self.out = Box::new(std::io::sink());
    }

    /// A line an action writes on stdout as it runs, as it wrote it.
    pub fn display(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
    }

    /// A line the reference prints in its debug colour: the `ASYNC POLL`, `ASYNC OK` and
    /// `ASYNC FAILED` lines of a polled job.
    pub fn debug(&mut self, line: &str) {
        let painted = self.paint(DEBUG, line);
        let _ = writeln!(self.out, "{painted}");
    }

    /// The line that follows a failure `ignore_errors` swallowed. A looping task prints it on its
    /// own, with no result line before it: the reference shows the items and then this, never an
    /// aggregate line of its own.
    pub fn ignoring(&mut self) {
        let _ = writeln!(self.out, "{}", self.paint(SKIPPED, "...ignoring"));
    }

    /// What `set_stats` collected, after the recap: each host that asked for `per_host`, sorted,
    /// then the run's own after a blank line. Nothing at all when nothing was collected.
    pub fn custom_stats(&mut self, stats: &CustomStats) {
        if stats.is_empty() {
            return;
        }
        let dump = |map: &serde_json::Map<String, serde_json::Value>| {
            dumps_indented(&serde_json::Value::Object(map.clone()), 0, 1).replace('\n', "")
        };
        self.banner("CUSTOM STATS:");
        let run = stats.run();
        for (host, data) in stats.hosts() {
            let _ = writeln!(self.out, "	{host}: {}", dump(data));
        }
        if !run.is_empty() {
            let _ = writeln!(self.out);
            let _ = writeln!(self.out, "	RUN: {}", dump(run));
        }
        let _ = writeln!(self.out);
    }

    pub fn recap(&mut self, stats: &Stats) {
        self.banner("PLAY RECAP");
        for (host, h) in stats.hosts() {
            let host_style = if h.failed > 0 || h.unreachable > 0 {
                FAILED
            } else if h.changed > 0 {
                CHANGED
            } else {
                OK
            };
            let name = if self.color {
                let painted = self.paint(host_style, host);
                let width = HOST_COLUMN + (painted.len() - host.len());
                format!("{painted:<width$}")
            } else {
                format!("{host:<HOST_COLUMN$}")
            };
            let field = |lead: &str, n: u32, style: Style| {
                let text = format!("{lead}={n:<4}");
                if n > 0 {
                    self.paint(style, &text)
                } else {
                    text
                }
            };
            let _ = writeln!(
                self.out,
                "{name} : {} {} {} {} {} {} {}",
                field("ok", h.ok, OK),
                field("changed", h.changed, CHANGED),
                field("unreachable", h.unreachable, UNREACHABLE),
                field("failed", h.failed, FAILED),
                field("skipped", h.skipped, SKIPPED),
                field("rescued", h.rescued, OK),
                field("ignored", h.ignored, SKIPPED),
            );
        }
        let _ = writeln!(self.out);
    }
}

#[cfg(test)]
mod tests;
