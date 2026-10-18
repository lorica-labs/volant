// SPDX-License-Identifier: GPL-3.0-or-later
//! Running and finding programs the way the reference does, and the words it uses to describe a
//! command that failed.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde_json::Value;

/// `AnsibleModule._clean_args(args)`, the `cmd` of a failed `run_command`: the word after one
/// that starts `pass` (after up to two `-`) masked, or the part after its `=`, every other word
/// run through `heuristic_log_sanitize`, then each word `shlex.quote`d.
pub fn clean_args(words: &[&str]) -> String {
    let mut clean = Vec::new();
    let mut masked = false;
    for word in words {
        if masked {
            masked = false;
            clean.push("********".to_string());
            continue;
        }
        let bare = word
            .strip_prefix("--")
            .or_else(|| word.strip_prefix('-'))
            .unwrap_or(word);
        if bare.starts_with("pass") {
            if let Some(at) = word.find('=') {
                clean.push(format!("{}=********", &word[..at]));
                continue;
            }
            masked = true;
        }
        clean.push(heuristic_log_sanitize(word));
    }
    clean
        .iter()
        .map(|word| shlex_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `heuristic_log_sanitize` without `no_log_values`: the password of every `user:password@host`
/// and `scheme://user:password@host` in `data` becomes `********`. It also eats text that only
/// looks like one, as the reference does.
pub(super) fn heuristic_log_sanitize(data: &str) -> String {
    let data: Vec<char> = data.chars().collect();
    let find = |needle: &[char], from: usize, to: usize| -> Option<usize> {
        let to = to.min(data.len());
        (from..to).find(|&i| i + needle.len() <= to && data[i..i + needle.len()] == *needle)
    };
    let rfind = |needle: &[char], to: usize| -> Option<usize> {
        let to = to.min(data.len());
        (0..to)
            .rev()
            .find(|&i| i + needle.len() <= to && data[i..i + needle.len()] == *needle)
    };
    // Pieces in the order the reference inserts them at the front: reversed at the end.
    let mut pieces: Vec<String> = Vec::new();
    let text = |from: usize, to: usize| data[from..to].iter().collect::<String>();
    let mut begin = data.len();
    let mut prev_begin = begin;
    loop {
        let Some(end) = rfind(&['@'], begin) else {
            pieces.push(text(0, begin));
            break;
        };
        let mut sep = None;
        let mut sep_search_end = end;
        while sep.is_none() {
            begin = rfind(&[':', '/', '/'], sep_search_end).unwrap_or(0);
            sep = find(&[':'], begin + 3, end);
            if sep.is_none() {
                if begin == 0 {
                    pieces.push(text(0, prev_begin));
                    break;
                }
                sep_search_end = begin;
            }
        }
        let Some(sep) = sep else { break };
        pieces.push(text(end, prev_begin));
        pieces.push("********".into());
        pieces.push(text(begin, sep + 1));
        prev_begin = begin;
    }
    pieces.reverse();
    pieces.concat()
}

/// The `exception` `fail_json(exception=ex)` sends for the `OSError` `Popen` raised, measured on
/// ansible-core 2.19.12 (tracebacks off, the default): an error summary whose chained event is
/// `str(ex)`. That text is Python's `[Errno N] strerror`, with the program as bytes when `exec`
/// failed; a failed `fork` (`EAGAIN`, `ENOMEM`) names no file. The wording comes from this libc,
/// so the chained message is not byte for byte the host Python's for every errno.
pub fn popen_error(errno: i32, program: &str) -> Value {
    let text = io::Error::from_raw_os_error(errno).to_string();
    let strerror = match strerror(errno) {
        Some(strerror) => strerror,
        None => text.split(" (os error").next().unwrap_or(&text),
    };
    let cause = if errno == libc::EAGAIN || errno == libc::ENOMEM {
        format!("[Errno {errno}] {strerror}")
    } else {
        format!("[Errno {errno}] {strerror}: {}", bytes_repr(program))
    };
    serde_json::json!({
        "__ansible_type": "ErrorSummary",
        "event": {
            "__ansible_type": "Event",
            "msg": "Error executing command.",
            "chain": {
                "__ansible_type": "EventChain",
                "msg_reason": "<<< caused by >>>",
                "traceback_reason": "The above exception was the direct cause of the following error:",
                "follow": true,
                "event": {"__ansible_type": "Event", "msg": cause},
            },
        },
    })
}

/// `shlex.quote`.
pub fn shlex_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\"'\"'"))
    }
}

/// glibc's `strerror` for the errors file operations meet.
pub fn strerror(errno: i32) -> Option<&'static str> {
    Some(match errno {
        libc::EPERM => "Operation not permitted",
        libc::ENOENT => "No such file or directory",
        libc::EIO => "Input/output error",
        libc::EACCES => "Permission denied",
        libc::EBUSY => "Device or resource busy",
        libc::EEXIST => "File exists",
        libc::ENOTDIR => "Not a directory",
        libc::EISDIR => "Is a directory",
        libc::ENOSPC => "No space left on device",
        libc::EROFS => "Read-only file system",
        libc::ENAMETOOLONG => "File name too long",
        libc::ENOTEMPTY => "Directory not empty",
        libc::ELOOP => "Too many levels of symbolic links",
        libc::EDQUOT => "Disk quota exceeded",
        _ => return None,
    })
}

/// Python's `repr()` of `text` encoded as bytes.
pub fn bytes_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut repr = format!("b{quote}");
    for byte in text.bytes() {
        match byte {
            b'\\' => repr.push_str("\\\\"),
            b'\t' => repr.push_str("\\t"),
            b'\n' => repr.push_str("\\n"),
            b'\r' => repr.push_str("\\r"),
            _ if byte == quote as u8 => {
                repr.push('\\');
                repr.push(quote);
            }
            0x20..0x7f => repr.push(byte as char),
            _ => {
                let _ = write!(repr, "\\x{byte:02x}");
            }
        }
    }
    repr.push(quote);
    repr
}

/// `get_bin_path`: the first executable `name` on `PATH`, then in the `sbin` directories.
pub fn bin_path(name: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut dirs: Vec<&str> = path.split(':').collect();
    for sbin in ["/sbin", "/usr/sbin", "/usr/local/sbin"] {
        if !dirs.contains(&sbin) && Path::new(sbin).exists() {
            dirs.push(sbin);
        }
    }
    dirs.into_iter()
        .filter(|dir| !dir.is_empty())
        .map(|dir| format!("{}/{name}", dir.trim_end_matches('/')))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|meta| !meta.is_dir() && meta.mode() & 0o111 != 0)
        })
}

/// Where `subprocess` finds `program`: as given with a `/`, otherwise along `PATH`, or
/// `/bin:/usr/bin` without one.
pub fn exec_path(program: &str) -> Option<String> {
    let runnable = |path: &str| {
        fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return runnable(program).then(|| program.to_string());
    }
    std::env::var("PATH")
        .unwrap_or_else(|_| "/bin:/usr/bin".into())
        .split(':')
        .map(|dir| format!("{}/{program}", if dir.is_empty() { "." } else { dir }))
        .find(|candidate| runnable(candidate))
}

/// Why a `validate` holding `%s` would not run as the reference runs `validate % path`, or
/// `Ok` when it would. `what` names the path in the reason.
pub fn validate_runs_as_python(validate: &str, path: &str, what: &str) -> Result<(), String> {
    // `validate % path` is Python's formatting, and `run_command` expands `~` and `$VAR` in
    // every argument.
    if validate.matches('%').count() != 1 || validate.contains(['$', '~']) {
        return Err("validate is formatted or expanded by Python".into());
    }
    // Where the `shlex` crate and Python's `shlex.split` part ways: the crate reads `#` as a
    // comment, drops a backslash before a backtick in double quotes, and keeps a carriage return
    // inside a word.
    if validate.contains(['#', '`', '\r', '\\']) {
        return Err("validate would not split as Python splits it".into());
    }
    if !path
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
    {
        return Err(format!("{what} would be split or expanded in validate"));
    }
    let argv = shlex::split(&validate.replace("%s", path))
        .ok_or("validate does not split as a command line")?;
    let program = argv.first().ok_or("validate names no program")?;
    if exec_path(program).is_none() {
        return Err("the validate program cannot be run".into());
    }
    Ok(())
}
