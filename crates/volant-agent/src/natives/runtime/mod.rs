// SPDX-License-Identifier: GPL-3.0-or-later
//! What the natives that run programs share: the task's clock and the reasons an answer stops,
//! the root a collector reads under, `PATH` lookup and program runs under the module's own
//! environment, and Python's string conversions.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use serde_json::{Map, Value};

use crate::modules::command::CANCEL_POLL;
use crate::modules::{Context, Run};

/// Why collecting ended without the facts.
#[derive(Debug)]
pub enum Stop {
    /// Outside what the native reproduces: the payload runs instead.
    HandBack(String),
    /// A command the native ran outlived the task's `timeout`.
    TimedOut,
    /// The controller cancelled the task while a command was running.
    Cancelled,
}

impl From<String> for Stop {
    fn from(reason: String) -> Stop {
        Stop::HandBack(reason)
    }
}

impl From<&str> for Stop {
    fn from(reason: &str) -> Stop {
        Stop::HandBack(reason.to_string())
    }
}

/// What bounds every command the native runs: the task's deadline and the controller's cancel,
/// honoured by the executor `command` runs under.
#[derive(Clone, Copy)]
pub struct Clock<'a> {
    pub deadline: Option<Instant>,
    pub cancelled: &'a dyn Fn() -> bool,
}

/// A task with no `timeout` that nothing cancels.
pub fn unbounded() -> Clock<'static> {
    fn never() -> bool {
        false
    }
    Clock {
        deadline: None,
        cancelled: &never,
    }
}

/// The next message on `answer`, waiting as long as it takes while polling the task's cancel,
/// which only this thread may ask. On the cancel, raises `halt` so that the runs whose clock
/// reads it stop, and says `Cancelled`. `None` when the sender is gone without a message.
pub(super) fn wait_polling_cancel<T>(
    answer: &mpsc::Receiver<T>,
    clock: Clock,
    halt: &AtomicBool,
) -> Result<Option<T>, Stop> {
    loop {
        match answer.recv_timeout(CANCEL_POLL) {
            Ok(message) => return Ok(Some(message)),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if (clock.cancelled)() {
                    halt.store(true, Ordering::Relaxed);
                    return Err(Stop::Cancelled);
                }
            }
        }
    }
}

/// Where the collectors read: `/` on a host, a directory in a test.
pub struct Root(String);

impl Root {
    pub fn real() -> Root {
        Root(String::new())
    }

    #[cfg(test)]
    pub fn at(dir: &Path) -> Root {
        Root(dir.to_string_lossy().into_owned())
    }

    /// `path` under the root, spelled as given: a trailing `/` stays, and with it the reference's
    /// "is a directory" meaning of `os.path.exists('/etc/init/')`.
    pub fn path(&self, path: &str) -> PathBuf {
        PathBuf::from(format!("{}{path}", self.0))
    }

    pub fn exists(&self, path: &str) -> bool {
        self.path(path).exists()
    }

    pub fn is_file(&self, path: &str) -> bool {
        self.path(path).is_file()
    }

    /// `get_file_content(path)`: the text, stripped, or `None` when the file is missing,
    /// unreadable, not UTF-8 or blank.
    pub fn content(&self, path: &str) -> Option<String> {
        let bytes = std::fs::read(self.path(path)).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        // Python reads text with universal newlines.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let stripped = py_strip(&text);
        (!stripped.is_empty()).then(|| stripped.to_string())
    }

    /// `get_file_lines(path)`.
    pub fn lines(&self, path: &str) -> Vec<String> {
        self.content(path)
            .map(|text| splitlines(&text).into_iter().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

/// `module.get_bin_path(name)`: the first executable file of that name along `PATH`, with
/// `/sbin`, `/usr/sbin` and `/usr/local/sbin` added when missing.
pub fn bin_path(root: &Root, env: &BTreeMap<String, String>, name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<String> = env
        .get("PATH")
        .map(String::as_str)
        .unwrap_or_default()
        .split(':')
        .map(str::to_string)
        .collect();
    for sbin in ["/sbin", "/usr/sbin", "/usr/local/sbin"] {
        if !dirs.iter().any(|dir| dir == sbin) && root.exists(sbin) {
            dirs.push(sbin.to_string());
        }
    }
    dirs.iter()
        .filter(|dir| !dir.is_empty())
        .map(|dir| root.path(&join(dir, name)))
        .find(|path| is_executable_file(path))
}

/// Where `subprocess` finds `name` when it is given without a directory: `PATH`, or
/// `/bin:/usr/bin` without one, and no extra directory.
pub(super) fn exec_path(
    root: &Root,
    env: &BTreeMap<String, String>,
    name: &str,
) -> Option<PathBuf> {
    env.get("PATH")
        .map_or("/bin:/usr/bin", String::as_str)
        .split(':')
        .map(|dir| root.path(&join(dir, name)))
        .find(|path| is_executable_file(path))
}

/// `module.run_command([program, args...])`, through the executor `command` runs under, so the
/// task's `timeout` and cancel reach it: the exit code and standard output, or `None` when the
/// program cannot be started, which the module reports rather than answering.
///
/// `env` is set over the agent's own environment, of which the module's is a superset.
pub fn run(
    env: &BTreeMap<String, String>,
    clock: Clock,
    program: &Path,
    args: &[&str],
) -> Result<Option<(i32, String)>, Stop> {
    Ok(run_output(env, clock, program, args)?
        .ok()
        .map(|(rc, stdout, _)| (rc, stdout)))
}

/// [`run`], with the standard error as well, and, for a program that could not be started, the
/// `errno` Python's `OSError` would carry.
pub fn run_output(
    env: &BTreeMap<String, String>,
    clock: Clock,
    program: &Path,
    args: &[&str],
) -> Result<Result<(i32, String, String), i32>, Stop> {
    let timeout = match clock.deadline {
        Some(deadline) => Some(
            deadline
                .checked_duration_since(Instant::now())
                .ok_or(Stop::TimedOut)?,
        ),
        None => None,
    };
    let mut argv = vec![Value::from(program.to_string_lossy().into_owned())];
    argv.extend(args.iter().map(|arg| Value::from(*arg)));
    let mut command = Map::new();
    command.insert("argv".into(), Value::Array(argv));
    command.insert("strip_empty_ends".into(), Value::Bool(false));
    let context = Context {
        timeout,
        environment: env.clone(),
        ..Context::default()
    };
    match crate::modules::command::execute(&command, false, &context, clock.cancelled) {
        Run::Cancelled => Err(Stop::Cancelled),
        Run::Done(result) if result.0.contains_key("timedout") => Err(Stop::TimedOut),
        // Only a command that started has a `start`.
        Run::Done(result) if result.0.contains_key("start") => Ok(Ok((
            result.0["rc"]
                .as_i64()
                .and_then(|rc| i32::try_from(rc).ok())
                .unwrap_or(-1),
            result.0["stdout"].as_str().unwrap_or_default().to_string(),
            result.0["stderr"].as_str().unwrap_or_default().to_string(),
        ))),
        Run::Done(result) => Ok(Err(result.0["rc"]
            .as_i64()
            .and_then(|rc| i32::try_from(rc).ok())
            .unwrap_or(1))),
    }
}

/// `os.path.join(dir, name)` for a plain name.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// `os.path.exists(path) and not os.path.isdir(path) and is_executable(path)`.
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| !meta.is_dir() && meta.permissions().mode() & 0o111 != 0)
}

/// Python's `str.isspace` for one character: Rust's whitespace plus the four separators
/// `\x1c`-`\x1f`.
pub fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// `str.strip()`.
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(py_space)
}

/// `str.split()`.
pub fn py_split(text: &str) -> impl Iterator<Item = &str> {
    text.split(py_space).filter(|word| !word.is_empty())
}

/// `int(text, radix)` for radix 10 or 16: surrounding whitespace, a sign, a `0x` prefix in base
/// 16, underscores between digits. `None` where Python raises, or beyond `i64`.
pub fn py_int(text: &str, radix: u32) -> Option<i64> {
    let text = py_strip(text);
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let mut digits = rest;
    if radix == 16
        && let Some(after) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X"))
    {
        digits = after.strip_prefix('_').unwrap_or(after);
    }
    let well_formed = !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits.chars().all(|c| c == '_' || c.is_digit(radix));
    if !well_formed {
        return None;
    }
    let value = i64::from_str_radix(&digits.replace('_', ""), radix).ok()?;
    Some(if negative { -value } else { value })
}

/// `str.splitlines()`.
pub fn splitlines(text: &str) -> Vec<&str> {
    let breaks = |c| {
        matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        )
    };
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(breaks) {
        lines.push(&rest[..at]);
        let width = if rest[at..].starts_with("\r\n") {
            2
        } else {
            rest[at..].chars().next().map_or(1, char::len_utf8)
        };
        rest = &rest[at + width..];
    }
    if !rest.is_empty() {
        lines.push(rest);
    }
    lines
}
