// SPDX-License-Identifier: GPL-3.0-or-later
//! Flow collections with lines left of the block that holds them, which PyYAML reads and saphyr
//! refuses.
//!
//! YAML 1.2 wants every line of a `[...]` or `{...}` indented past the block that holds it, so
//! saphyr fails `labels: [\n  "bug"\n]\nbody: 1` with `invalid indentation` at the `]`. PyYAML
//! does not look at indentation inside a flow collection, and playbooks close one at the column
//! of its key. Blanks before a token there mean nothing, so indenting those lines further leaves
//! the value as it was.

use anyhow::Error;

/// `text` with the lines of the flow collection `err` refused for its indentation indented one
/// column past the key it is the value of, or `None` when `err` is not that refusal.
pub(super) fn reindented(text: &str, err: &Error) -> Option<String> {
    let scan = err
        .root_cause()
        .downcast_ref::<saphyr_parser::ScanError>()?;
    if !matches!(
        scan.info(),
        "invalid indentation" | "invalid indentation in flow construct"
    ) {
        return None;
    }
    // saphyr counts characters there, not bytes.
    let at = text
        .char_indices()
        .nth(scan.marker().index())
        .map_or(text.len(), |(at, _)| at);
    let refused = line_start(text, at);
    let indent = blanks(&text[refused..]);
    // The line the collection opens on: the nearest one above, neither blank nor a comment, that
    // is not indented deeper.
    let mut opens = refused;
    loop {
        opens = line_start(text, opens.checked_sub(1)?);
        let line = &text[opens..];
        let first = line.trim_start_matches(' ').bytes().next();
        if !matches!(first, Some(b'\n' | b'#')) && blanks(line) <= indent {
            break;
        }
    }
    let (open, close) = collection(text, opens)?;
    if !(open < at && at <= close) {
        return None;
    }
    let line = &text[opens..];
    let mut key = blanks(line);
    while line[key..].starts_with("- ") {
        key += 2 + blanks(&line[key + 2..]);
    }
    let want = key + 1;
    let first = opens + text[opens..].find('\n')? + 1;
    let last = text[close..].find('\n').map_or(text.len(), |n| close + n);
    let mut out = String::with_capacity(text.len() + 64);
    out.push_str(&text[..first]);
    for (i, line) in text[first..last].split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let have = blanks(line);
        // A document marker at the first column ends the collection for PyYAML too.
        let marker = line.starts_with("---") || line.starts_with("...");
        if have < want && !line.trim().is_empty() && !marker {
            out.extend(std::iter::repeat_n(' ', want - have));
        }
        out.push_str(line);
    }
    out.push_str(&text[last..]);
    (out != text).then_some(out)
}

/// The offset of the start of the line `at` is on.
fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |n| n + 1)
}

/// The number of spaces `line` starts with.
fn blanks(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// The offsets of the `[` or `{` that opens a flow collection on the line starting at `from` and
/// of the bracket that closes it, skipping quoted scalars and comments.
fn collection(text: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut open = None;
    let mut depth = 0usize;
    // The last byte that is not a blank: a bracket or a quote only starts a node after these.
    let mut prev = b'\n';
    let mut at = from;
    while at < bytes.len() {
        let b = bytes[at];
        let starts_node = matches!(prev, b'\n' | b':' | b'-' | b',' | b'[' | b'{');
        match b {
            b'"' | b'\'' if starts_node => {
                at += 1 + super::quoted::closing(&text[at + 1..], b)?;
            }
            b'#' if at == 0 || matches!(bytes[at - 1], b' ' | b'\t' | b'\n') => {
                at = text[at..].find('\n').map_or(bytes.len(), |n| at + n);
                continue;
            }
            b'[' | b'{' if open.is_some() || starts_node => {
                open.get_or_insert(at);
                depth += 1;
            }
            b']' | b'}' if open.is_some() => {
                depth -= 1;
                if depth == 0 {
                    return Some((open?, at));
                }
            }
            b'\n' if open.is_none() => return None,
            _ => {}
        }
        if !matches!(b, b' ' | b'\t') {
            prev = b;
        }
        at += 1;
    }
    None
}
