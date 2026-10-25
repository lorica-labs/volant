// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

const HEADER: &str = r#"{"event": "header", "format": 1, "ansible_core": "2.19.12", "cli": {}, "cwd": "/w", "playbooks": [], "inventory": {"sources": [], "files": []}, "extra_vars_hash": "x", "started": "s"}"#;

fn lines(n: usize) -> String {
    let tasks = (0..n).map(|i| {
        format!("{{\"event\": \"task\", \"uuid\": \"u{i}\", \"name\": \"task number {i}\"}}\n")
    });
    std::iter::once(format!("{HEADER}\n"))
        .chain(tasks)
        .collect()
}

/// One gzip member around `text`, with a file name in its header as Python's `gzip` writes
/// none and other tools do. The CRC is left at zero: it is not read.
fn gzip(text: &str, name: bool) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, if name { 8 } else { 0 }, 0, 0, 0, 0, 0, 0xff];
    if name {
        out.extend_from_slice(b"run.jsonl\0");
    }
    out.extend(miniz_oxide::deflate::compress_to_vec(text.as_bytes(), 6));
    out.extend([0; 4]);
    out.extend((text.len() as u32).to_le_bytes());
    out
}

#[test]
fn a_whole_recording_is_read() {
    let recording = parse(lines(3).as_bytes(), false).unwrap();
    assert_eq!(recording.header.ansible_core, "2.19.12");
    assert_eq!(recording.events.len(), 3);
    assert!(!recording.truncated);
}

#[test]
fn a_last_line_cut_short_is_dropped_and_said() {
    let mut text = lines(2);
    text.push_str(r#"{"event": "task", "uu"#);
    let recording = parse(text.as_bytes(), false).unwrap();
    assert_eq!(recording.events.len(), 2);
    assert!(recording.truncated);
}

#[rstest::rstest]
fn gzip_is_read_with_or_without_a_file_name(#[values(false, true)] name: bool) {
    let (text, cut) = gunzip(&gzip(&lines(5), name)).unwrap();
    assert_eq!(String::from_utf8(text).unwrap(), lines(5));
    assert!(!cut);
}

/// A stream a killed run left unfinished gives back the lines before the cut.
#[test]
fn an_unfinished_gzip_stream_keeps_what_came_before() {
    let whole = gzip(&lines(2000), false);
    let (text, cut) = gunzip(&whole[..whole.len() * 3 / 5]).unwrap();
    assert!(cut);
    let recording = parse(&text, cut).unwrap();
    assert!(recording.truncated);
    assert!(recording.events.len() > 100, "{}", recording.events.len());
}

#[test]
fn a_newer_format_is_refused_naming_both() {
    let text = HEADER.replace("\"format\": 1", "\"format\": 2") + "\n";
    let err = parse(text.as_bytes(), false).unwrap_err();
    assert!(
        err.contains("format 2") && err.contains("format 1"),
        "{err}"
    );
}

#[rstest::rstest]
#[case::no_header("{\"event\": \"task\"}\n", "not a header")]
#[case::empty("", "empty")]
#[case::not_json("{\n", "not JSON")]
#[case::no_event(&format!("{HEADER}\n{{\"x\": 1}}\n"), "line 2 has no event")]
fn a_broken_recording_says_why(#[case] text: &str, #[case] why: &str) {
    let err = parse(text.as_bytes(), false).unwrap_err();
    assert!(err.contains(why), "{err}");
}

#[test]
fn a_file_that_is_not_gzip_is_named_so() {
    assert_eq!(
        gunzip(b"plain text, not gzip").unwrap_err(),
        "not a gzip file"
    );
}
