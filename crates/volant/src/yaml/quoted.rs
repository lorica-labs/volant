// SPDX-License-Identifier: GPL-3.0-or-later
//! Quoted scalars whose continuation lines sit left of their parent, which PyYAML reads and
//! saphyr refuses.
//!
//! YAML 1.2 wants every line of a multi-line flow scalar indented past the node that holds it, so
//! saphyr fails `k: "{{\n  x\n}}"` with `invalid indentation in quoted scalar`. PyYAML only refuses
//! a document marker there, and roles rely on it: `xanmanning.k3s` closes its templated variables
//! with `}}"` at the first column. The leading blanks of a continuation line are folded away in
//! both quoting styles, so indenting those lines further leaves the value as it was.

use anyhow::Error;

/// What saphyr says of a quoted scalar with a continuation line left of its parent.
const INVALID_INDENT: &str = "invalid indentation in quoted scalar";

/// `text` with the continuation lines of the quoted scalar `err` refused for its indentation
/// indented one column past the line it opens on, or `None` when `err` is not that refusal.
pub(super) fn reindented(text: &str, err: &Error) -> Option<String> {
    let scan = err
        .root_cause()
        .downcast_ref::<saphyr_parser::ScanError>()?;
    if scan.info() != INVALID_INDENT {
        return None;
    }
    // saphyr counts characters there, not bytes.
    let open = text.char_indices().nth(scan.marker().index())?.0;
    let quote = *text.as_bytes().get(open)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let close = open + 1 + closing(&text[open + 1..], quote)?;
    let line_start = text[..open].rfind('\n').map_or(0, |at| at + 1);
    let want = text[line_start..].len() - text[line_start..].trim_start_matches(' ').len() + 1;
    let mut out = String::with_capacity(text.len() + 64);
    out.push_str(&text[..open]);
    let mut lines = text[open..=close].split('\n');
    out.push_str(lines.next()?);
    for line in lines {
        out.push('\n');
        let have = line.len() - line.trim_start_matches(' ').len();
        // A document marker at the first column ends the scalar for PyYAML too.
        let marker = line.starts_with("---") || line.starts_with("...");
        if have < want && !line.trim().is_empty() && !marker {
            out.extend(std::iter::repeat_n(' ', want - have));
        }
        out.push_str(line);
    }
    out.push_str(&text[close + 1..]);
    (out != text).then_some(out)
}

/// The offset in `rest` of the quote that closes a scalar opened by `quote`: a double-quoted one
/// escapes with a backslash, a single-quoted one writes its quote twice.
fn closing(rest: &str, quote: u8) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' if quote == b'"' => at += 2,
            b'\'' if quote == b'\'' && bytes.get(at + 1) == Some(&b'\'') => at += 2,
            b if b == quote => return Some(at),
            _ => at += 1,
        }
    }
    None
}
