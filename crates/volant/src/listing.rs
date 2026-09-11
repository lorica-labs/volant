// SPDX-License-Identifier: GPL-3.0-or-later
//! What `--list-tasks`, `--list-tags`, `--list-hosts` and `--syntax-check` print.
//!
//! The four of them read a playbook and print what running it would do, without connecting to
//! anything. That makes them the cheapest check an operator has, and the widest one this engine
//! has of its own compilation: the text below is compared to ansible-core's, byte for byte, over
//! a corpus of playbooks in `tests/golden/listing`, so a role spliced in the wrong order or a
//! tag inherited from the wrong place shows up as a diff rather than as a behaviour nobody
//! looked at.
//!
//! The layout is not invented. It is ansible-core 2.19.12's own, measured down to the
//! whitespace: a blank line before each `playbook:` header, a tab before every `TAGS:`, six
//! spaces in front of a task, a comma **and a space** between a task's tags and a comma alone
//! between a play's.
//!
//! Two deliberate divergences, both forced by the reference printing a Python `set`:
//!
//! - a play with more than one tag prints them there in an order that changes between
//!   processes; this prints them sorted;
//! - `--list-hosts` prints the hosts of a play from a `set` too, so a play matching more than
//!   one host lists them in an order that changes between processes; this prints them in
//!   inventory order.
//!
//! Both were measured by running the same command repeatedly: four distinct orders in eight
//! runs for three hosts. Sorting is the only answer that can be compared with anything.

use crate::compile::{self, Compiled};
use crate::playbook::Play;

/// Which of the four the command line asked for. They combine the way the reference combines
/// them, measured: `--syntax-check` prints its own short form and nothing else, while
/// `--list-hosts` and `--list-tasks` together print both sections under one play header.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Listing {
    pub tasks: bool,
    pub tags: bool,
    pub hosts: bool,
    pub syntax: bool,
}

impl Listing {
    /// Whether the run is a listing rather than a run. A listing connects to nothing, so it
    /// skips the pre-flight: a playbook whose keywords or modules this release cannot execute
    /// still lists, which is the whole point of being able to read one before the release that
    /// runs it.
    pub fn wanted(&self) -> bool {
        self.tasks || self.tags || self.hosts || self.syntax
    }
}

/// One play as the listing sees it.
pub(crate) struct PlayEntry<'a> {
    pub play: &'a Play,
    pub compiled: &'a Compiled,
    /// The hosts the play would run on, in inventory order, with `--limit` applied.
    pub hosts: Vec<String>,
}

/// One playbook argument, with the plays that came out of it - `import_playbook` included, which
/// is why the numbering runs across the file rather than restarting.
pub(crate) struct PlaybookEntry<'a> {
    /// The argument exactly as it was typed. Measured: the reference echoes it unchanged, so
    /// `./x.yml` prints as `./x.yml`.
    pub argument: String,
    pub plays: Vec<PlayEntry<'a>>,
}

/// The reference's `Display.display`: the text, plus a newline unless it already ends in one.
fn line(out: &mut String, text: &str) {
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
}

pub(crate) fn render(entries: &[PlaybookEntry], mode: Listing) -> String {
    let mut out = String::new();
    for entry in entries {
        line(&mut out, &format!("\nplaybook: {}", entry.argument));
        // `--syntax-check` stops here: the reference loads and validates every play and then
        // prints nothing about them. Ours loads, compiles and splices roles the same way, so a
        // playbook it cannot make sense of is refused with the same code - it just does not
        // resolve module names, which is what lets a role compile before its modules ship.
        if mode.syntax {
            continue;
        }
        for (index, entry) in entry.plays.iter().enumerate() {
            let play = entry.play;
            let mut msg = format!(
                "\n  play #{} ({}): {}\tTAGS: [{}]",
                index + 1,
                play.hosts,
                play.name,
                play.tags.join(",")
            );
            if mode.hosts {
                msg.push_str(&format!(
                    "\n    pattern: {}\n    hosts ({}):",
                    patterns(&play.host_patterns),
                    entry.hosts.len()
                ));
                for host in &entry.hosts {
                    msg.push_str(&format!("\n      {host}"));
                }
            }
            line(&mut out, &msg);
            if !(mode.tasks || mode.tags) {
                continue;
            }
            let mut body = if mode.tasks {
                "    tasks:\n".to_string()
            } else {
                String::new()
            };
            // Every tag of every step that survived the selection, plus the play's own: what
            // `--list-tags` prints is the tags of the tasks it would actually run.
            let mut all: Vec<&str> = play.tags.iter().map(String::as_str).collect();
            for (index, step) in entry.compiled.steps.iter().enumerate() {
                // A task written in a `rescue:` or an `always:` is listed nowhere and its tags
                // reach no `TASK TAGS` line: measured, the reference walks a block's body and
                // stops there.
                if !compile::listed(entry.compiled, index) {
                    continue;
                }
                all.extend(step.task.tags.iter().map(String::as_str));
                if mode.tasks {
                    body.push_str(&format!(
                        "      {}\tTAGS: [{}]\n",
                        label(step, entry.compiled),
                        step.task.tags.join(", ")
                    ));
                }
            }
            if mode.tags {
                all.sort_unstable();
                all.dedup();
                body.push_str(&format!("      TASK TAGS: [{}]\n", all.join(", ")));
            }
            line(&mut out, &body);
        }
    }
    out
}

/// A play's `hosts` value as the reference prints it: a Python list of the entries as written.
/// Measured on ansible-core 2.19.12: `hosts: a,b` prints `['a,b']` - one pattern - while
/// `hosts: [a, b]` prints `['a', 'b']`, so the two cannot be told apart from the joined string
/// the rest of the engine uses and the written entries are kept beside it.
fn patterns(written: &[String]) -> String {
    if written.is_empty() {
        return "[]".to_string();
    }
    format!("['{}']", written.join("', '"))
}

/// How one step is named in `--list-tasks`.
///
/// Measured on ansible-core 2.19.12: a named task of a role reads `role : name`, an **unnamed**
/// one reads its module alone with no role prefix (`debug`, not `spec : debug`), and a module
/// written in full keeps the spelling the playbook used (`ansible.builtin.debug`). A name
/// holding a template is printed as written, never rendered: no host has been chosen yet.
///
/// This is deliberately not the banner, which prefixes an unnamed role task too
/// (`TASK [spec : debug]`). The two were measured separately because they differ.
fn label(step: &crate::compile::Step, compiled: &Compiled) -> String {
    if !step.task.named {
        return step.task.name.clone();
    }
    match step.role.and_then(|i| compiled.roles.get(i)) {
        Some(role) => format!("{} : {}", role.name, step.task.name),
        None => step.task.name.clone(),
    }
}
