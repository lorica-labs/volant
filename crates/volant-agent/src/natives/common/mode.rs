// SPDX-License-Identifier: GPL-3.0-or-later
//! File modes: the octal and symbolic forms of `mode`, and the process `umask`.

use std::fs;

use serde_json::Value;

/// Why a `mode` gives no permission bits.
#[derive(Debug, PartialEq)]
pub enum ModeError {
    /// The reference fails with `mode must be in octal or symbolic form`; this is its `details`.
    Invalid(String),
    /// A form the reference reads some other way (`0o755`, `+755`, a float, `True`...): the
    /// Python module decides.
    Unsupported(String),
}

/// The permission bits `mode` asks for, as `set_mode_if_different` computes them: an integer
/// as it is, a string of octal digits in base 8, anything else as a symbolic mode
/// (`_symbolic_mode_to_octal`) applied to `current`, the path's permission bits now.
pub fn parse_mode(mode: &Value, current: u32, is_dir: bool) -> Result<u32, ModeError> {
    let unsupported =
        || ModeError::Unsupported("mode takes a form the module reads its own way".into());
    match mode {
        Value::Number(n) => n
            .as_u64()
            .filter(|bits| *bits <= 0o7777)
            .map(|bits| bits as u32)
            .ok_or_else(unsupported),
        Value::String(text)
            if !text.is_empty() && text.bytes().all(|b| (b'0'..=b'7').contains(&b)) =>
        {
            u32::from_str_radix(text, 8)
                .ok()
                .filter(|bits| *bits <= 0o7777)
                .ok_or_else(unsupported)
        }
        Value::String(text)
            if text.bytes().all(|b| b.is_ascii_graphic())
                && !(text.bytes().any(|b| b.is_ascii_digit())
                    && text
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b"_+-oO".contains(&b))) =>
        {
            symbolic_mode(text, current, is_dir)
        }
        _ => Err(unsupported()),
    }
}

/// `_symbolic_mode_to_octal`: `u=rwx,g+X,o-w` and the like, each clause applied in turn.
fn symbolic_mode(mode: &str, current: u32, is_dir: bool) -> Result<u32, ModeError> {
    let mut new = current;
    for clause in mode.split(',') {
        let invalid = || ModeError::Invalid(format!("bad symbolic permission for mode: {clause}"));
        let mut parts = clause.split(['+', '=', '-']);
        let operators: Vec<char> = clause.chars().filter(|c| "+=-".contains(*c)).collect();
        let users = parts.next().unwrap_or_default();
        let use_umask = users.is_empty();
        let users = if users.is_empty() || users == "a" {
            "ugo"
        } else {
            users
        };
        if !users.chars().all(|c| "ugo".contains(c)) {
            return Err(invalid());
        }
        for (perms, operator) in parts.zip(operators) {
            if !perms.chars().all(|c| "rwxXstugo".contains(c)) {
                return Err(invalid());
            }
            let umask = if use_umask { umask()? } else { 0 };
            for user in users.chars() {
                let bits = perms.chars().fold(0, |acc, perm| {
                    let masked = if "rwx".contains(perm) { !umask } else { 0o7777 };
                    acc | (perm_bits(user, perm, new, is_dir) & masked)
                });
                new = match operator {
                    '=' => {
                        let mask = match user {
                            'u' => 0o4700,
                            'g' => 0o2070,
                            _ => 0o1007,
                        };
                        (new & (mask ^ 0o7777)) | bits
                    }
                    '+' => new | bits,
                    _ => new & !bits,
                };
            }
        }
    }
    Ok(new)
}

/// What one letter of a symbolic mode means for `user`, `prev` being the bits so far
/// (`_get_octal_mode_from_symbolic_perms`).
fn perm_bits(user: char, perm: char, prev: u32, is_dir: bool) -> u32 {
    // Where `user`'s three bits sit: 6 for u, 3 for g, 0 for o.
    let shift = match user {
        'u' => 6,
        'g' => 3,
        _ => 0,
    };
    let copy = |from: u32| ((prev >> from) & 0o7) << shift;
    match perm {
        'r' => 0o4 << shift,
        'w' => 0o2 << shift,
        'x' => 0o1 << shift,
        'X' if is_dir || prev & 0o111 != 0 => 0o1 << shift,
        's' if user == 'u' => 0o4000,
        's' if user == 'g' => 0o2000,
        't' if user == 'o' => 0o1000,
        'u' => copy(6),
        'g' => copy(3),
        'o' => copy(0),
        _ => 0,
    }
}

/// The process umask, read without setting it (`/proc/self/status`, Linux 4.7 and later): the
/// agent's threads would see a temporary `umask(0)`.
pub fn umask() -> Result<u32, ModeError> {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let value = status
                .lines()
                .find_map(|line| line.strip_prefix("Umask:"))?;
            u32::from_str_radix(value.trim(), 8).ok()
        })
        .ok_or_else(|| ModeError::Unsupported("the umask cannot be read".into()))
}
