// SPDX-License-Identifier: GPL-3.0-or-later
//! `log_path`: what the run displays, written to a file as ansible-core's logger writes it.
//!
//! Measured on ansible-core 2.19.12: one record per message the run displays, `<date> <time>,<ms>
//! p=<pid> u=<user> n=ansible <LEVEL>| <message>`, the message without its leading blank lines or
//! colours, and the lines after its first written as they are. A `[WARNING]` is `WARNING`, an
//! `[ERROR]` is `ERROR` and everything else, deprecations included, is `INFO`. A line shown only
//! from a number of `-v`s is logged when `log_verbosity` reaches that number, shown or not.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

struct Log {
    file: Mutex<std::fs::File>,
    user: String,
    verbosity: u8,
}

static LOG: OnceLock<Log> = OnceLock::new();

/// Opens `path` for the run, appending, as the reference's `FileHandler` does. A file that
/// cannot be opened is said in the reference's warning and the run goes on without a log.
pub fn open(path: &std::path::Path, verbosity: u8) -> Result<(), String> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|_| {
            format!(
                "log file at '{}' is not writeable and we cannot create it, aborting",
                path.display()
            )
        })?;
    let user = ["LOGNAME", "USER", "LNAME", "USERNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    let _ = LOG.set(Log {
        file: Mutex::new(file),
        user,
        verbosity,
    });
    Ok(())
}

/// Whether a line shown from `level` `-v`s up goes to the log.
pub fn logs_level(level: u8) -> bool {
    LOG.get().is_some_and(|log| log.verbosity >= level)
}

/// Writes one displayed message to the log, when there is one: `message` is what the screen
/// was given, and the log ends it with a newline of its own, as the reference's logger does.
pub fn record(message: &str) {
    let Some(log) = LOG.get() else {
        return;
    };
    let plain = strip_ansi(message);
    let text = plain.trim_start_matches('\n');
    if text.trim().is_empty() {
        return;
    }
    let level = if text.starts_with("[WARNING]") {
        "WARNING"
    } else if text.starts_with("[ERROR]") {
        "ERROR"
    } else {
        "INFO"
    };
    let stamp = crate::template::local_now("%Y-%m-%d %H:%M:%S,%f");
    let stamp = &stamp[..stamp.len().saturating_sub(3)];
    let line = format!(
        "{stamp} p={} u={} n=ansible {level}| {text}\n",
        std::process::id(),
        log.user
    );
    let mut file = log
        .file
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = file.write_all(line.as_bytes());
}

/// Writes text that holds several displayed messages, each opening with its `[WARNING]:`,
/// `[DEPRECATION WARNING]:` or `[ERROR]:`, as one record each.
pub fn record_each(text: &str) {
    let mut start = 0;
    for (at, _) in text.match_indices('\n') {
        let next = &text[at + 1..];
        if ["[WARNING]:", "[DEPRECATION WARNING]:", "[ERROR]:"]
            .iter()
            .any(|p| next.starts_with(p))
        {
            record(&text[start..=at]);
            start = at + 1;
        }
    }
    record(&text[start..]);
}

/// `text` without the colour sequences the terminal is given.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A writer that logs each message written to it whole: a `writeln!` is one record.
pub struct Tee(pub Box<dyn Write>);

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }

    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
        let text = args.to_string();
        // The newline a `writeln!` ends the line with is the screen's, not the message's.
        record(text.strip_suffix('\n').unwrap_or(&text));
        self.0.write_all(text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn colours_are_left_out_of_the_log() {
        assert_eq!(
            super::strip_ansi("\u{1b}[0;32mok: [h1]\u{1b}[0m"),
            "ok: [h1]"
        );
    }
}
