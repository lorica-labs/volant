// SPDX-License-Identifier: GPL-3.0-or-later
//! The `-vvvv` lines of the `ssh` commands the controller runs to reach a host, in the shape of
//! the reference's `ESTABLISH SSH CONNECTION`, `SSH: EXEC` and result lines. They name the
//! commands this controller runs (the probe, the upload, the link to the agent), never the
//! reference's own.

use std::fmt::Write as _;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

use anstream::{AutoStream, ColorChoice};

use super::bootstrap::Captured;
use super::defaults::SshTarget;

/// The level the connection lines show from: the reference prints them one step lower, but
/// they describe other commands, so they stay with the rest of the `-vvvv` detail.
const LEVEL_FROM: u8 = 4;

static VERBOSITY: AtomicU8 = AtomicU8::new(0);
static COLOR: Mutex<ColorChoice> = Mutex::new(ColorChoice::Auto);

/// Sets how many `-v`s the run was given and the colour choice, once, before any link opens.
pub fn configure(verbosity: u8, choice: ColorChoice) {
    VERBOSITY.store(verbosity, Ordering::Relaxed);
    *COLOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = choice;
}

fn enabled() -> bool {
    VERBOSITY.load(Ordering::Relaxed) >= LEVEL_FROM
}

fn line(text: &str) {
    let choice = *COLOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out = AutoStream::new(std::io::stdout(), choice);
    let style = crate::render::VERBOSE;
    let _ = writeln!(out, "{style}{text}{style:#}");
}

/// `<address> ESTABLISH SSH CONNECTION FOR USER: <user>`, `None` when no user is set.
pub(super) fn established(target: &SshTarget) {
    if enabled() {
        line(&format!(
            "<{}> ESTABLISH SSH CONNECTION FOR USER: {}",
            target.address,
            target.user.as_deref().unwrap_or("None")
        ));
    }
}

/// `<address> SSH: EXEC <the command line>`.
pub(super) fn exec(target: &SshTarget, argv: &[String]) {
    if enabled() {
        let words: Vec<String> = argv.iter().map(|word| shell_quote(word)).collect();
        line(&format!(
            "<{}> SSH: EXEC {}",
            target.address,
            words.join(" ")
        ));
    }
}

/// `<address> (code, b'stdout', b'stderr')` once a command that was waited for has ended.
pub(super) fn result(target: &SshTarget, captured: &Captured) {
    if enabled() {
        line(&format!(
            "<{}> ({}, {}, {})",
            target.address,
            captured.code.unwrap_or(-1),
            bytes_repr(captured.stdout.as_bytes()),
            bytes_repr(captured.stderr.as_bytes())
        ));
    }
}

/// `shlex.quote`: a word of safe characters stays bare, any other is single-quoted.
fn shell_quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\"'\"'"))
    }
}

/// A Python `bytes` literal for `bytes`: the quote Python picks, and its escapes.
fn bytes_repr(bytes: &[u8]) -> String {
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    let mut text = format!("b{quote}");
    for &byte in bytes {
        match byte {
            b'\\' => text.push_str("\\\\"),
            b'\t' => text.push_str("\\t"),
            b'\n' => text.push_str("\\n"),
            b'\r' => text.push_str("\\r"),
            b if char::from(b) == quote => {
                text.push('\\');
                text.push(quote);
            }
            0x20..=0x7e => text.push(char::from(byte)),
            _ => {
                let _ = write!(text, "\\x{byte:02x}");
            }
        }
    }
    text.push(quote);
    text
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::{bytes_repr, shell_quote};

    #[rstest]
    #[case(b"ok\n", "b'ok\\n'")]
    #[case(b"it's", "b\"it's\"")]
    #[case(b"a\\b\x01\xc3", "b'a\\\\b\\x01\\xc3'")]
    #[case(b"", "b''")]
    fn a_bytes_literal_reads_as_python_prints_it(#[case] bytes: &[u8], #[case] want: &str) {
        assert_eq!(bytes_repr(bytes), want);
    }

    #[rstest]
    #[case("-o", "-o")]
    #[case("/bin/sh -c 'x'", "'/bin/sh -c '\"'\"'x'\"'\"''")]
    #[case("", "''")]
    fn a_word_is_quoted_as_shlex_quotes_it(#[case] word: &str, #[case] want: &str) {
        assert_eq!(shell_quote(word), want);
    }
}
