// SPDX-License-Identifier: GPL-3.0-or-later
//! Console output in the shape of ansible-playbook's default callback.

use std::io::Write;

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use volant_protocol::TaskResult;

use crate::stats::{Outcome, Stats};

/// Whether a result's body goes on its line at verbosity 0, the reference's
/// `_ansible_verbose_always`, and how it is cleaned first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dump {
    /// Shown from `-v` on, like any task's.
    No,
    /// A `debug`: always shown, stripped to the message the way the reference's callback
    /// strips a `debug` result.
    Debug,
    /// Always shown as it is: an `assert` that is not `quiet`.
    Whole,
}

pub struct Renderer {
    out: Box<dyn Write>,
    color: bool,
    width: usize,
    verbosity: u8,
}

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
        }
    }

    #[cfg(test)]
    pub fn with_writer(out: Box<dyn Write>, color: bool, width: usize, verbosity: u8) -> Self {
        Self {
            out,
            color,
            width,
            verbosity,
        }
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

    pub fn task(&mut self, name: &str) {
        self.banner(&format!("TASK [{name}]"));
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
    pub fn warning(&mut self, text: &str, censored: bool) {
        let body = if censored { CENSORED } else { text };
        let _ = writeln!(
            anstream::stderr(),
            "{}",
            self.paint(WARNING, &format!("[WARNING]: {body}"))
        );
    }

    /// The line a failed attempt prints before the task is reported, measured on ansible-core
    /// 2.19.12: it goes to stdout from verbosity 0, it names the task rather than the module,
    /// and it is printed after **every** failed attempt, the last one included.
    pub fn retrying(&mut self, host: &str, name: &str, left: u32) {
        let _ = writeln!(
            self.out,
            "FAILED - RETRYING: [{host}]: {name} ({left} retries left)."
        );
    }

    /// `label` is the loop item's display text, present only for a loop item result. `dump`
    /// forces the JSON tail even for an `ok` result at verbosity 0; see [`Dump`].
    ///
    /// `censored` is the task's `no_log`: the body becomes `{"censored": CENSORED, "changed":
    /// ...}` and the item label becomes [`CENSORED_ITEM`], which is what keeps a secret out of
    /// every line this function can print - the ordinary one, the `fatal:`, the loop item and
    /// the `debug`, at every verbosity. What it does **not** touch is the result itself: the
    /// registered variable and the recap read the real one, measured.
    #[expect(
        clippy::too_many_arguments,
        reason = "one task result's whole display context: the outcome plus four parameters that change what gets printed - two flags (dump, censored) and two optional strings (label, delegate)"
    )]
    pub fn result(
        &mut self,
        host: &str,
        outcome: Outcome,
        result: &TaskResult,
        label: Option<&str>,
        dump: Dump,
        censored: bool,
        delegate: Option<&str>,
    ) {
        let censored_body = || {
            let mut body = serde_json::Map::new();
            body.insert("censored".into(), serde_json::json!(CENSORED));
            body.insert("changed".into(), serde_json::json!(result.changed()));
            body
        };
        let label = if censored {
            label.map(|_| CENSORED_ITEM)
        } else {
            label
        };
        let mut body = if censored {
            censored_body()
        } else {
            result.0.clone()
        };
        // The reference shows no `failed` at all; a `true` stays until that is matched.
        if body.get("failed") == Some(&serde_json::Value::Bool(false)) {
            body.remove("failed");
        }
        if dump == Dump::Debug {
            // Ansible cleans a `debug` result before showing it, so the message stands alone:
            // whatever `changed_when` and `failed_when` decided is counted, never printed, and
            // neither is the attempt count - measured on ansible-core 2.19.12, a `debug` retried
            // twice shows `fatal: [localhost]: FAILED! => {"msg": "probe"}` while its registered
            // value keeps `attempts: 2`.
            body.retain(|k, _| {
                !matches!(
                    k.as_str(),
                    "changed"
                        | "failed"
                        | "skipped"
                        | "failed_when_result"
                        | "invocation"
                        | "attempts"
                )
            });
        }
        let json = ansible_json(&serde_json::Value::Object(body));
        let item = label.map(|l| format!(" => (item={l})")).unwrap_or_default();
        // A censored `debug` still goes through the cleanup above - which is what leaves its
        // body as `{"censored": ...}` with no `changed` - but shows nothing at verbosity 0:
        // measured, `ok: [h1]` alone there and the censored body from `-v` on.
        let show = (dump != Dump::No && !censored) || self.verbosity > 0;
        let tail = if show {
            format!(" => {json}")
        } else {
            String::new()
        };
        // `[h1 -> h3]` wherever a delegated task has a line, measured on ansible-core 2.19.12 -
        // `ok:`, `changed:`, the `fatal:` of a failure and a loop item's own line all carry the
        // arrow. `skipping:` is the one that does not: a task a `when` left out never reached
        // the delegate, and the reference prints `skipping: [h1]` for it.
        let who = match delegate {
            Some(d) => format!("{host} -> {d}"),
            None => host.to_string(),
        };
        let line = match outcome {
            Outcome::Ok => self.paint(OK, &format!("ok: [{who}]{item}{tail}")),
            Outcome::Changed => self.paint(CHANGED, &format!("changed: [{who}]{item}{tail}")),
            Outcome::Skipped => self.paint(SKIPPED, &format!("skipping: [{host}]{item}")),
            // A rescued failure shows exactly like one nothing catches: measured on
            // ansible-core 2.19.12, the line is the same `fatal: ... FAILED!` and the only
            // difference is in the recap. Showing it any other way would hide from the
            // operator that the task failed at all.
            Outcome::Failed | Outcome::Rescued | Outcome::Ignored if label.is_some() => self.paint(
                FAILED,
                &format!(
                    "failed: [{who}] (item={}) => {json}",
                    label.unwrap_or_default()
                ),
            ),
            Outcome::Failed | Outcome::Rescued | Outcome::Ignored => {
                self.paint(FAILED, &format!("fatal: [{who}]: FAILED! => {json}"))
            }
        };
        let _ = writeln!(self.out, "{line}");
        if outcome == Outcome::Ignored && label.is_none() {
            self.ignoring();
        }
    }

    /// The line that follows a failure `ignore_errors` swallowed. A looping task prints it on its
    /// own, with no result line before it: the reference shows the items and then this, never an
    /// aggregate line of its own.
    pub fn ignoring(&mut self) {
        let _ = writeln!(self.out, "{}", self.paint(SKIPPED, "...ignoring"));
    }

    /// `censored` is the `no_log` of the task whose batch could not be sent. Measured on
    /// ansible-core 2.19.12: the reference censors this line too, so the reason the host could
    /// not be reached is hidden along with everything else the task would have printed.
    pub fn unreachable(&mut self, host: &str, msg: &str, censored: bool, delegate: Option<&str>) {
        let body = if censored {
            serde_json::json!({"censored": CENSORED, "changed": false})
        } else {
            serde_json::json!({"changed": false, "msg": msg, "unreachable": true})
        };
        let who = match delegate {
            Some(d) => format!("{host} -> {d}"),
            None => host.to_string(),
        };
        let line = format!("fatal: [{who}]: UNREACHABLE! => {}", ansible_json(&body));
        let _ = writeln!(self.out, "{}", self.paint(UNREACHABLE, &line));
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

/// Ansible dumps results as JSON with sorted keys and `": "` and `", "` separators.
pub(crate) fn ansible_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap_or_default(),
                        ansible_json(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(ansible_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_with_color(color: bool, f: impl FnOnce(&mut Renderer)) -> String {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut r = Renderer::with_writer(Box::new(Shared(buf.clone())), color, 79, 0);
        f(&mut r);
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    fn capture(f: impl FnOnce(&mut Renderer)) -> String {
        capture_with_color(false, f)
    }

    fn result(v: serde_json::Value) -> TaskResult {
        TaskResult(v.as_object().unwrap().clone())
    }

    /// Strips ANSI CSI sequences (`ESC [ ... letter`), the shape anstyle emits, so a
    /// coloured capture can be compared against the plain one on visible text alone.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn banners_are_padded_with_stars_past_the_width() {
        let out = capture(|r| r.play("Smoke test"));
        let line = out.lines().nth(1).unwrap();
        assert!(line.starts_with("PLAY [Smoke test] ***"));
        assert_eq!(line.len(), 80);
        assert_eq!(out, format!("\n{line}\n"));
    }

    #[test]
    fn banners_count_characters_not_bytes() {
        let out = capture(|r| r.play("Déploiement été"));
        assert_eq!(out.lines().nth(1).unwrap().chars().count(), 80);
    }

    #[test]
    fn a_banner_wider_than_the_terminal_keeps_three_stars() {
        let name = "x".repeat(120);
        let out = capture(|r| r.play(&name));
        assert_eq!(out.lines().nth(1).unwrap(), format!("PLAY [{name}] ***"));
    }

    #[test]
    fn task_results_use_ansible_prefixes() {
        let out = capture(|r| {
            r.task("Say hello");
            r.result(
                "web1",
                Outcome::Changed,
                &result(json!({"changed": true, "stdout": "hi"})),
                None,
                Dump::No,
                false,
                None,
            );
            r.result(
                "web2",
                Outcome::Ok,
                &result(json!({"changed": false})),
                None,
                Dump::No,
                false,
                None,
            );
            r.result(
                "web3",
                Outcome::Skipped,
                &result(json!({"skipped": true})),
                None,
                Dump::No,
                false,
                None,
            );
            r.result(
                "web4",
                Outcome::Failed,
                &result(json!({"failed": true, "rc": 1, "msg": "non-zero return code"})),
                None,
                Dump::No,
                false,
                None,
            );
            r.result(
                "web5",
                Outcome::Ignored,
                &result(json!({"failed": true, "rc": 1})),
                None,
                Dump::No,
                false,
                None,
            );
        });
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].starts_with("TASK [Say hello] ***"));
        assert_eq!(lines[2], "changed: [web1]");
        assert_eq!(lines[3], "ok: [web2]");
        assert_eq!(lines[4], "skipping: [web3]");
        assert_eq!(
            lines[5],
            r#"fatal: [web4]: FAILED! => {"failed": true, "msg": "non-zero return code", "rc": 1}"#
        );
        assert_eq!(
            lines[6],
            r#"fatal: [web5]: FAILED! => {"failed": true, "rc": 1}"#
        );
        assert_eq!(lines[7], "...ignoring");
    }

    #[test]
    fn unreachable_and_no_hosts_have_their_lines() {
        let out = capture(|r| {
            r.unreachable("db1", "agent binary not found", false, None);
            r.no_hosts();
        });
        assert_eq!(
            out.lines().next().unwrap(),
            r#"fatal: [db1]: UNREACHABLE! => {"changed": false, "msg": "agent binary not found", "unreachable": true}"#
        );
        assert_eq!(out.lines().nth(1).unwrap(), "skipping: no hosts matched");
    }

    #[test]
    fn the_recap_matches_ansible_columns() {
        let mut stats = Stats::default();
        stats.record("localhost", Outcome::Changed, true);
        stats.record("localhost", Outcome::Ok, false);
        stats.record("localhost", Outcome::Ignored, false);
        let out = capture(|r| r.recap(&stats));
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].starts_with("PLAY RECAP ***"));
        assert_eq!(
            lines[2],
            "localhost                  : ok=3    changed=1    unreachable=0    failed=0    skipped=0    rescued=0    ignored=1   "
        );
    }

    #[test]
    fn verbose_mode_prints_results_for_ok_tasks_too() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut r = Renderer::with_writer(Box::new(Shared(buf.clone())), false, 79, 1);
        r.result(
            "h",
            Outcome::Ok,
            &result(json!({"changed": false, "stdout": "x"})),
            None,
            Dump::No,
            false,
            None,
        );
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(
            out.trim_end(),
            r#"ok: [h] => {"changed": false, "stdout": "x"}"#
        );
    }

    /// The `failed: false` every result that ran now carries is for `register`, not for the
    /// screen: measured on ansible-core 2.19.12, `-v` shows `ok: [localhost] => {"changed":
    /// false, "ping": "pong"}` for a registered value that reads `failed=False`.
    ///
    /// What would make this red: the filled-in key printed, a line the reference never shows.
    #[test]
    fn a_result_that_did_not_fail_does_not_say_so() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut r = Renderer::with_writer(Box::new(Shared(buf.clone())), false, 79, 1);
        r.result(
            "h",
            Outcome::Ok,
            &result(json!({"changed": false, "failed": false, "ping": "pong"})),
            None,
            Dump::No,
            false,
            None,
        );
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(
            out.trim_end(),
            r#"ok: [h] => {"changed": false, "ping": "pong"}"#
        );
    }

    #[test]
    fn the_recap_still_lines_up_with_colour_on() {
        let mut stats = Stats::default();
        stats.record("localhost", Outcome::Changed, true);
        stats.record("localhost", Outcome::Ok, false);
        stats.record("localhost", Outcome::Ignored, false);
        let plain = capture(|r| r.recap(&stats));
        let coloured = capture_with_color(true, |r| r.recap(&stats));
        assert_ne!(coloured, plain, "colour should change the output at all");
        assert_eq!(
            strip_ansi(&coloured),
            plain,
            "visible columns must match the uncoloured recap once escapes are stripped"
        );
    }

    #[test]
    fn result_lines_only_get_wrapped_in_colour_not_rewritten() {
        let cases = [
            (Outcome::Ok, result(json!({"changed": false}))),
            (Outcome::Changed, result(json!({"changed": true}))),
            (Outcome::Skipped, result(json!({"skipped": true}))),
            (Outcome::Failed, result(json!({"failed": true, "rc": 1}))),
            (Outcome::Ignored, result(json!({"failed": true, "rc": 1}))),
        ];
        for (outcome, task_result) in cases {
            let plain =
                capture(|r| r.result("h", outcome, &task_result, None, Dump::No, false, None));
            let coloured = capture_with_color(true, |r| {
                r.result("h", outcome, &task_result, None, Dump::No, false, None);
            });
            assert_ne!(
                coloured, plain,
                "{outcome:?}: colour should change the output at all"
            );
            assert_eq!(
                strip_ansi(&coloured),
                plain,
                "{outcome:?}: visible text must be unchanged by colour"
            );
            for line in plain.lines() {
                assert!(
                    coloured.contains(line),
                    "{outcome:?}: styling should wrap {line:?} verbatim, not rewrite it"
                );
            }
        }
    }

    /// Every line a `no_log` result can put on the terminal, at verbosity 0 and at `-v`, in the
    /// reference's own words - measured on ansible-core 2.19.12 with `nolog.yml`.
    ///
    /// What would make this red: a secret reaching any of these lines, which is the whole point
    /// of the keyword; or the censored `debug` printing its body at verbosity 0, where the
    /// reference prints `ok: [h1]` alone.
    #[test]
    fn a_censored_result_shows_the_same_body_on_every_line_it_can_print() {
        let secret = result(json!({"changed": true, "stdout": "secret"}));
        let failed = result(json!({"changed": true, "failed": true, "stdout": "secret"}));
        let out = capture(|r| {
            r.result("h1", Outcome::Changed, &secret, None, Dump::No, true, None);
            r.result("h1", Outcome::Ignored, &failed, None, Dump::No, true, None);
            r.result(
                "h1",
                Outcome::Changed,
                &secret,
                Some("a"),
                Dump::No,
                true,
                None,
            );
            r.result(
                "h1",
                Outcome::Ok,
                &result(json!({"changed": false, "msg": "hush"})),
                None,
                Dump::Debug,
                true,
                None,
            );
        });
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "changed: [h1]");
        assert_eq!(
            lines[1],
            format!(r#"fatal: [h1]: FAILED! => {{"censored": "{CENSORED}", "changed": true}}"#)
        );
        assert_eq!(lines[2], "...ignoring");
        assert_eq!(lines[3], "changed: [h1] => (item=(censored due to no_log))");
        assert_eq!(lines[4], "ok: [h1]");
        assert!(!out.contains("secret"), "{out}");

        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut r = Renderer::with_writer(Box::new(Shared(buf.clone())), false, 79, 1);
        r.result("h1", Outcome::Changed, &secret, None, Dump::No, true, None);
        r.result(
            "h1",
            Outcome::Ok,
            &result(json!({"changed": false, "msg": "hush"})),
            None,
            Dump::Debug,
            true,
            None,
        );
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            format!(r#"changed: [h1] => {{"censored": "{CENSORED}", "changed": true}}"#)
        );
        assert_eq!(
            lines[1],
            format!(r#"ok: [h1] => {{"censored": "{CENSORED}"}}"#)
        );
        assert!(!out.contains("secret") && !out.contains("hush"), "{out}");
    }

    /// The retry line, in the reference's own words and on stdout.
    ///
    /// What would make this red: the count, the punctuation or the stream changed - the line is
    /// what an operator greps for while a playbook waits on a host that is not ready yet.
    #[test]
    fn the_retry_line_is_the_reference_s_own() {
        let out = capture(|r| r.retrying("h1", "retry until file", 3));
        assert_eq!(
            out,
            "FAILED - RETRYING: [h1]: retry until file (3 retries left).\n"
        );
    }

    /// An unreachable host under `no_log` says no more than any other censored line, measured.
    #[test]
    fn a_censored_unreachable_hides_its_reason_too() {
        let out = capture(|r| r.unreachable("h1", "starting ssh: secret-host", true, None));
        assert_eq!(
            out.trim_end(),
            format!(
                r#"fatal: [h1]: UNREACHABLE! => {{"censored": "{CENSORED}", "changed": false}}"#
            )
        );
    }

    #[test]
    fn loop_items_and_forced_dumps_follow_ansible_shapes() {
        let out = capture(|r| {
            r.result(
                "h",
                Outcome::Changed,
                &result(json!({"changed": true})),
                Some("one"),
                Dump::No,
                false,
                None,
            );
            r.result(
                "h",
                Outcome::Skipped,
                &result(json!({"skipped": true})),
                Some("two"),
                Dump::No,
                false,
                None,
            );
            r.result(
                "h",
                Outcome::Failed,
                &result(json!({"failed": true, "rc": 1})),
                Some("three"),
                Dump::No,
                false,
                None,
            );
            r.result(
                "h",
                Outcome::Ok,
                &result(json!({"msg": "shown"})),
                None,
                Dump::Debug,
                false,
                None,
            );
        });
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "changed: [h] => (item=one)");
        assert_eq!(lines[1], "skipping: [h] => (item=two)");
        assert_eq!(
            lines[2],
            r#"failed: [h] (item=three) => {"failed": true, "rc": 1}"#
        );
        assert_eq!(lines[3], r#"ok: [h] => {"msg": "shown"}"#);
    }
}
