// SPDX-License-Identifier: GPL-3.0-or-later
//! `~/.ansible-console_history`: read at the start, written whole at the end, as the reference's
//! `readline.read_history_file` and `write_history_file` do. A file libedit wrote (`_HiStOrY_V2_`
//! first, a space written `\040`) is read too; the file is written as GNU readline writes it, one
//! command per line, and created with mode `0600`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

pub(super) fn path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ansible-console_history"))
}

/// The commands of the file, oldest first; nothing when it cannot be read.
pub(super) fn load(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines = text.lines().peekable();
    let libedit = lines.peek() == Some(&"_HiStOrY_V2_");
    if libedit {
        lines.next();
    }
    lines
        .map(|line| {
            if libedit {
                unvis(line)
            } else {
                line.to_string()
            }
        })
        .collect()
}

/// libedit's `strvis` undone: `\ooo` octal escapes and `\\`.
fn unvis(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)));
        match (bytes[i], octal) {
            (b'\\', Some(digits)) => {
                let value = digits
                    .iter()
                    .fold(0_u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                out.push(u8::try_from(value).unwrap_or(b'?'));
                i += 4;
            }
            (b'\\', None) if bytes.get(i + 1) == Some(&b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Writes `lines` over the file, which a new file gets with mode `0600`.
pub(super) fn save(path: &Path, lines: &[String]) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(path) {
        let mut text = lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = file.write_all(text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_libedit_file_and_a_readline_file_both_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h");
        std::fs::write(&path, "_HiStOrY_V2_\nshell\\040echo\\040hi\nping\n").unwrap();
        assert_eq!(load(&path), ["shell echo hi", "ping"]);
        save(&path, &["cd web".into(), "ping".into()]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "cd web\nping\n");
        assert_eq!(load(&path), ["cd web", "ping"]);
    }
}
