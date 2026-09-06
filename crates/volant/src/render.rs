// SPDX-License-Identifier: GPL-3.0-or-later
//! Console output in the shape of ansible-playbook's default callback.

use std::io::Write;

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use volant_protocol::TaskResult;

use crate::stats::{Outcome, Stats};

pub struct Renderer {
    out: Box<dyn Write>,
    color: bool,
    width: usize,
    verbosity: u8,
}

const HOST_COLUMN: usize = 26;

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

    fn banner(&mut self, text: &str) {
        let stars = self.width.saturating_sub(text.len() + 1);
        let _ = writeln!(self.out, "\n{text} {}", "*".repeat(stars));
    }

    pub fn play(&mut self, name: &str) {
        self.banner(&format!("PLAY [{name}]"));
    }

    pub fn task(&mut self, name: &str) {
        self.banner(&format!("TASK [{name}]"));
    }

    pub fn no_hosts(&mut self) {
        let _ = writeln!(
            self.out,
            "{}",
            self.paint(SKIPPED, "skipping: no hosts matched")
        );
    }

    pub fn warning(&mut self, text: &str) {
        let _ = writeln!(
            self.out,
            "{}",
            self.paint(WARNING, &format!("[WARNING]: {text}"))
        );
    }

    pub fn result(&mut self, host: &str, outcome: Outcome, result: &TaskResult) {
        let dump = ansible_json(&serde_json::Value::Object(result.0.clone()));
        let line = match outcome {
            Outcome::Ok if self.verbosity > 0 => self.paint(OK, &format!("ok: [{host}] => {dump}")),
            Outcome::Ok => self.paint(OK, &format!("ok: [{host}]")),
            Outcome::Changed if self.verbosity > 0 => {
                self.paint(CHANGED, &format!("changed: [{host}] => {dump}"))
            }
            Outcome::Changed => self.paint(CHANGED, &format!("changed: [{host}]")),
            Outcome::Skipped => self.paint(SKIPPED, &format!("skipping: [{host}]")),
            Outcome::Failed | Outcome::Ignored => {
                self.paint(FAILED, &format!("fatal: [{host}]: FAILED! => {dump}"))
            }
        };
        let _ = writeln!(self.out, "{line}");
        if outcome == Outcome::Ignored {
            let _ = writeln!(self.out, "{}", self.paint(SKIPPED, "...ignoring"));
        }
    }

    pub fn unreachable(&mut self, host: &str, msg: &str) {
        let body = serde_json::json!({"changed": false, "msg": msg, "unreachable": true});
        let line = format!("fatal: [{host}]: UNREACHABLE! => {}", ansible_json(&body));
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
fn ansible_json(value: &serde_json::Value) -> String {
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
    impl std::io::Write for Shared {
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
    fn banners_are_padded_with_stars_to_the_width() {
        let out = capture(|r| r.play("Smoke test"));
        let line = out.lines().nth(1).unwrap();
        assert!(line.starts_with("PLAY [Smoke test] ***"));
        assert_eq!(line.len(), 79);
        assert_eq!(out, format!("\n{line}\n"));
    }

    #[test]
    fn task_results_use_ansible_prefixes() {
        let out = capture(|r| {
            r.task("Say hello");
            r.result(
                "web1",
                Outcome::Changed,
                &result(json!({"changed": true, "stdout": "hi"})),
            );
            r.result("web2", Outcome::Ok, &result(json!({"changed": false})));
            r.result("web3", Outcome::Skipped, &result(json!({"skipped": true})));
            r.result(
                "web4",
                Outcome::Failed,
                &result(json!({"failed": true, "rc": 1, "msg": "non-zero return code"})),
            );
            r.result(
                "web5",
                Outcome::Ignored,
                &result(json!({"failed": true, "rc": 1})),
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
            r.unreachable("db1", "agent binary not found");
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
        stats.record("localhost", Outcome::Changed);
        stats.record("localhost", Outcome::Ok);
        stats.record("localhost", Outcome::Ignored);
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
        );
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(
            out.trim_end(),
            r#"ok: [h] => {"changed": false, "stdout": "x"}"#
        );
    }

    #[test]
    fn the_recap_still_lines_up_with_colour_on() {
        let mut stats = Stats::default();
        stats.record("localhost", Outcome::Changed);
        stats.record("localhost", Outcome::Ok);
        stats.record("localhost", Outcome::Ignored);
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
            let plain = capture(|r| r.result("h", outcome, &task_result));
            let coloured = capture_with_color(true, |r| r.result("h", outcome, &task_result));
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
}
