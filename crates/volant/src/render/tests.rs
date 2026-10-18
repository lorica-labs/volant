// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use rstest::rstest;
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

fn capture_at(color: bool, verbosity: u8, f: impl FnOnce(&mut Renderer)) -> String {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut r = Renderer::with_writer(Box::new(Shared(buf.clone())), color, 79, verbosity);
    f(&mut r);
    String::from_utf8(buf.lock().unwrap().clone()).unwrap()
}

fn capture_with_color(color: bool, f: impl FnOnce(&mut Renderer)) -> String {
    capture_at(color, 0, f)
}

fn capture(f: impl FnOnce(&mut Renderer)) -> String {
    capture_with_color(false, f)
}

/// Each verbosity line shows from the level the reference gives it and not before.
#[rstest]
#[case::v(1, &[])]
#[case::vv(2, &["PLAYBOOK: p.yml ", "2 plays in dir/p.yml", "task path: /abs/p.yml:4"])]
#[case::vvvv(4, &["PLAYBOOK: p.yml ", "forks: 5", "2 plays in dir/p.yml", "task path: /abs/p.yml:4"])]
fn the_playbook_and_task_path_lines_show_at_vv(#[case] level: u8, #[case] shown: &[&str]) {
    let out = capture_at(false, level, |r| {
        r.playbook(
            std::path::Path::new("dir/p.yml"),
            2,
            &["forks: 5".to_string()],
        );
        r.task_path(&crate::playbook::Origin {
            path: "/abs/p.yml".into(),
            line: 4,
            col: 3,
        });
    });
    let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), shown.len(), "{out}");
    for (line, start) in lines.iter().zip(shown) {
        assert!(line.starts_with(start), "{line}");
    }
}

/// The source lines under `Origin:`, read the way Python's text mode reads them, and what
/// shows when the file cannot be read.
#[rstest]
#[case::crlf(b"a: 1\r\nb: 2\r\n".as_slice(), vec!["1 a: 1", "2 b: 2", "  ^ column 1"])]
#[case::lone_cr(b"a: 1\rb: 2\r".as_slice(), vec!["1 a: 1", "2 b: 2", "  ^ column 1"])]
#[case::not_utf8(b"a: \xff\nb: 2\n".as_slice(), vec!["(source not shown: UnicodeDecodeError)"])]
fn source_lines_read_as_python_does(#[case] bytes: &[u8], #[case] expected: Vec<&str>) {
    let dir = std::env::temp_dir().join(format!("volant-lines-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    let path = dir.join(format!("{}.yml", expected.len() + bytes.len()));
    std::fs::write(&path, bytes).expect("the source");
    let origin = crate::playbook::Origin {
        path: path.display().to_string().into(),
        line: 2,
        col: 1,
    };
    assert_eq!(annotated_lines(&origin), expected);
    let missing = crate::playbook::Origin {
        path: dir.join("missing.yml").display().to_string().into(),
        ..origin
    };
    assert_eq!(
        annotated_lines(&missing),
        ["(source not shown: FileNotFoundError)"]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn shorten_keeps_whole_words_as_textwrap_does() {
    assert_eq!(shorten("a  b\n c", 120), "a b c");
    assert_eq!(shorten("aaaa bbbb cccc dddd", 15), "aaaa bbbb [...]");
    assert_eq!(shorten("aaaaaaaaaaaaaaaaaaaa", 10), "[...]");
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

/// Banners pad with stars to the width, count characters and keep three past it; the recap,
/// the unreachable, no-hosts and retry lines are the reference's own.
#[test]
fn banners_recap_and_status_lines() {
    let out = capture(|r| r.play("Smoke test"));
    let line = out.lines().nth(1).unwrap();
    assert!(line.starts_with("PLAY [Smoke test] ***"));
    assert_eq!(line.len(), 80);
    assert_eq!(out, format!("\n{line}\n"));
    let out = capture(|r| r.play("Déploiement été"));
    assert_eq!(out.lines().nth(1).unwrap().chars().count(), 80);
    let name = "x".repeat(120);
    let out = capture(|r| r.play(&name));
    assert_eq!(out.lines().nth(1).unwrap(), format!("PLAY [{name}] ***"));
    let out = capture(|r| r.task("Say hello"));
    assert!(
        out.lines()
            .nth(1)
            .unwrap()
            .starts_with("TASK [Say hello] ***")
    );

    let out = capture(|r| {
        r.unreachable("db1", "agent binary not found", false, None);
        r.no_hosts();
        r.unreachable("h1", "starting ssh: secret-host", true, None);
        r.retrying(
            "h1",
            "retry until file",
            3,
            &result(json!({"rc": 1})),
            false,
        );
        let debug = result(
            json!({"_ansible_verbose_always": true, "attempts": 1, "failed": true, "msg": "probe", "retries": 3}),
        );
        r.retrying("h1", "probe", 2, &debug, false);
        r.retrying("h1", "hushed", 2, &debug, true);
    });
    assert_eq!(
        out,
        format!(
            "fatal: [db1]: UNREACHABLE! => {{\"changed\": false, \"msg\": \"agent binary not found\", \"unreachable\": true}}\nskipping: no hosts matched\nfatal: [h1]: UNREACHABLE! => {{\"censored\": \"{CENSORED}\", \"changed\": false}}\nFAILED - RETRYING: [h1]: retry until file (3 retries left).\nFAILED - RETRYING: [h1]: probe (2 retries left).Result was: {{\n    \"attempts\": 1,\n    \"msg\": \"probe\",\n    \"retries\": 3\n}}\nFAILED - RETRYING: [h1]: hushed (2 retries left).\n"
        )
    );

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

/// The line a result prints, in the reference's words; `<CENSORED>` stands for the
/// censored sentence.
#[rstest]
#[case::changed(0, Outcome::Changed, json!({"changed": true, "stdout": "hi"}), None, Dump::No, false, "changed: [h]\n")]
#[case::ok(0, Outcome::Ok, json!({"changed": false}), None, Dump::No, false, "ok: [h]\n")]
#[case::skipped(0, Outcome::Skipped, json!({"skipped": true}), None, Dump::No, false, "skipping: [h]\n")]
#[case::failed(
    0, Outcome::Failed, json!({"failed": true, "rc": 1, "msg": "non-zero return code"}), None, Dump::No, false,
    "fatal: [h]: FAILED! => {\"msg\": \"non-zero return code\", \"rc\": 1}\n"
)]
#[case::ignored(
    0, Outcome::Ignored, json!({"failed": true, "rc": 1}), None, Dump::No, false,
    "fatal: [h]: FAILED! => {\"rc\": 1}\n...ignoring\n"
)]
#[case::exception_stays_off_the_line(
    0, Outcome::Failed,
    json!({"changed": false, "exception": "(traceback unavailable)", "failed": true, "msg": "Error executing command.", "rc": 2}),
    None, Dump::No, false,
    "fatal: [h]: FAILED! => {\"changed\": false, \"msg\": \"Error executing command.\", \"rc\": 2}\n"
)]
#[case::failed_diff_and_invocation_stay_off(
    0, Outcome::Failed,
    json!({"changed": false, "diff": [], "failed": true, "invocation": {"module_args": {"path": "/tmp/x"}}, "msg": "boom"}),
    None, Dump::No, false,
    "fatal: [h]: FAILED! => {\"changed\": false, \"msg\": \"boom\"}\n"
)]
#[case::verbose_ok(
    1, Outcome::Ok, json!({"changed": false, "stdout": "x"}), None, Dump::No, false,
    "ok: [h] => {\"changed\": false, \"stdout\": \"x\"}\n"
)]
#[case::verbose_ok_does_not_say_it_did_not_fail(
    1, Outcome::Ok, json!({"changed": false, "failed": false, "ping": "pong"}), None, Dump::No, false,
    "ok: [h] => {\"changed\": false, \"ping\": \"pong\"}\n"
)]
#[case::changed_item(0, Outcome::Changed, json!({"changed": true}), Some("one"), Dump::No, false, "changed: [h] => (item=one)\n")]
#[case::skipped_item(0, Outcome::Skipped, json!({"skipped": true}), Some("two"), Dump::No, false, "skipping: [h] => (item=two) \n")]
#[case::skipped_item_verbose(
    1, Outcome::Skipped, json!({"changed": false, "item": 2, "skipped": true}), Some("2"), Dump::No, false,
    "skipping: [h] => (item=2)  => {\"changed\": false, \"item\": 2}\n"
)]
#[case::failed_item(
    0, Outcome::Failed, json!({"failed": true, "rc": 1}), Some("three"), Dump::No, false,
    "failed: [h] (item=three) => {\"rc\": 1}\n"
)]
#[case::forced_dump(0, Outcome::Ok, json!({"_ansible_verbose_always": true, "msg": "shown"}), None, Dump::Debug, false, "ok: [h] => {\n    \"msg\": \"shown\"\n}\n")]
#[case::debug_var_keeps_its_item(
    0, Outcome::Ok, json!({"_ansible_verbose_always": true, "ansible_loop_var": "item", "changed": false, "item": "a"}), Some("a"), Dump::Debug, false,
    "ok: [h] => (item=a) => {\n    \"item\": \"a\"\n}\n"
)]
#[case::raised_failure_on_one_line(
    0, Outcome::Failed, json!({"changed": false, "failed": true, "msg": "Task failed: x"}), None, Dump::Debug, false,
    "fatal: [h]: FAILED! => {\"msg\": \"Task failed: x\"}\n"
)]
#[case::censored_keeps_attempts(
    0, Outcome::Failed, json!({"attempts": 2, "changed": false, "failed": true, "msg": "secret"}), None, Dump::No, true,
    "fatal: [h]: FAILED! => {\"attempts\": 2, \"censored\": \"<CENSORED>\", \"changed\": false}\n"
)]
#[case::censored_changed(0, Outcome::Changed, json!({"changed": true, "stdout": "secret"}), None, Dump::No, true, "changed: [h]\n")]
#[case::censored_ignored(
    0, Outcome::Ignored, json!({"changed": true, "failed": true, "stdout": "secret"}), None, Dump::No, true,
    "fatal: [h]: FAILED! => {\"censored\": \"<CENSORED>\", \"changed\": true}\n...ignoring\n"
)]
#[case::censored_item(
    0, Outcome::Changed, json!({"changed": true, "stdout": "secret"}), Some("a"), Dump::No, true,
    "changed: [h] => (item=(censored due to no_log))\n"
)]
#[case::censored_debug(0, Outcome::Ok, json!({"_ansible_verbose_always": true, "changed": false, "msg": "hush"}), None, Dump::Debug, true, "ok: [h]\n")]
#[case::censored_changed_verbose(
    1, Outcome::Changed, json!({"changed": true, "stdout": "secret"}), None, Dump::No, true,
    "changed: [h] => {\"censored\": \"<CENSORED>\", \"changed\": true}\n"
)]
#[case::censored_debug_verbose(
    1, Outcome::Ok, json!({"_ansible_verbose_always": true, "changed": false, "msg": "hush"}), None, Dump::Debug, true,
    "ok: [h] => {\"censored\": \"<CENSORED>\"}\n"
)]
fn a_result_prints_the_reference_line(
    #[case] verbosity: u8,
    #[case] outcome: Outcome,
    #[case] body: serde_json::Value,
    #[case] item: Option<&str>,
    #[case] dump: Dump,
    #[case] censored: bool,
    #[case] expected: &str,
) {
    let out = capture_at(false, verbosity, |r| {
        r.result("h", outcome, &result(body), item, dump, censored, None);
    });
    assert_eq!(out, expected.replace("<CENSORED>", CENSORED));
}

/// Colour wraps the visible text and never rewrites it: the recap still lines up, and every
/// result line is contained verbatim in the coloured output.
#[test]
fn colour_only_wraps_what_is_printed() {
    let mut stats = Stats::default();
    stats.record("localhost", Outcome::Changed, true);
    stats.record("localhost", Outcome::Ok, false);
    stats.record("localhost", Outcome::Ignored, false);
    let plain = capture(|r| r.recap(&stats));
    let coloured = capture_with_color(true, |r| r.recap(&stats));
    assert_ne!(coloured, plain, "colour should change the output at all");
    assert_eq!(strip_ansi(&coloured), plain, "the recap columns");

    let cases = [
        (Outcome::Ok, result(json!({"changed": false}))),
        (Outcome::Changed, result(json!({"changed": true}))),
        (Outcome::Skipped, result(json!({"skipped": true}))),
        (Outcome::Failed, result(json!({"failed": true, "rc": 1}))),
        (Outcome::Ignored, result(json!({"failed": true, "rc": 1}))),
    ];
    for (outcome, task_result) in cases {
        let plain = capture(|r| r.result("h", outcome, &task_result, None, Dump::No, false, None));
        let coloured = capture_with_color(true, |r| {
            r.result("h", outcome, &task_result, None, Dump::No, false, None);
        });
        assert_ne!(
            coloured, plain,
            "{outcome:?}: colour should change the output"
        );
        assert_eq!(strip_ansi(&coloured), plain, "{outcome:?}: visible text");
        for line in plain.lines() {
            assert!(coloured.contains(line), "{outcome:?}: {line:?} rewritten");
        }
    }
}

/// The `set_stats` sequence of the scout case, on four hosts, prints the reference's block.
#[test]
fn custom_stats_print_per_host_lines_then_the_run_aggregate() {
    let mut stats = CustomStats::default();
    let sets = [
        json!({"data": {"total": 1, "flag": true}, "per_host": false, "aggregate": true}),
        json!({"data": {"cnt": 2}, "per_host": true, "aggregate": true}),
        json!({"data": {"cnt": 5}, "per_host": false, "aggregate": true}),
        json!({"data": {"cnt": 5}, "per_host": false, "aggregate": false}),
    ];
    for set in &sets {
        for host in ["a", "b", "c", "d"] {
            stats.record(host, set);
        }
    }
    let text = capture(|r| r.custom_stats(&stats));
    let want = format!(
        "
CUSTOM STATS: {}
	a: {{ \"cnt\": 2}}
	b: {{ \"cnt\": 2}}
	c: {{ \"cnt\": 2}}
	d: {{ \"cnt\": 2}}

	RUN: {{ \"cnt\": 5, \"flag\": 4, \"total\": 4}}

",
        "*".repeat(66)
    );
    assert_eq!(text, want);
    assert_eq!(capture(|r| r.custom_stats(&CustomStats::default())), "");
}

/// A result that prints no diff never starts the helper.
#[rstest]
#[case::no_diff(json!({"changed": true, "_volant_diff": true}))]
#[case::empty_list(json!({"changed": true, "diff": [], "_volant_diff": true}))]
#[case::unchanged(json!({"changed": false, "diff": {"before": {}, "after": {}}, "_volant_diff": true}))]
#[case::not_asked(json!({"changed": true, "diff": {"before": "a", "after": "b"}}))]
fn no_diff_starts_no_helper(#[case] result: serde_json::Value) {
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&starts);
    let bridge = crate::bridge::Bridge::with_start(Box::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::bail!("no helper in a unit test")
    }));
    let shown = capture(|r| {
        r.set_diffs(bridge);
        let result = TaskResult(result.as_object().unwrap().clone());
        r.result("h", Outcome::Changed, &result, None, Dump::No, false, None);
    });
    assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(shown.starts_with("changed: [h]"), "{shown}");
}
