// SPDX-License-Identifier: GPL-3.0-or-later
//! Whether at least one path matches a pattern, the way `glob.glob` answers it: `*`, `?` and
//! `[...]` match inside one path component and never across a `/`. `**` is not special, which
//! is what `glob.glob` does without `recursive=True`.

use std::path::{Path, PathBuf};

/// Whether anything matches `pattern`, resolved against `base` when it is relative.
pub(crate) fn matches_any(base: Option<&Path>, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let start = if pattern.starts_with('/') {
        PathBuf::from("/")
    } else {
        match base {
            Some(dir) => dir.to_path_buf(),
            // No `chdir`: the pattern resolves against the process's own directory, which is
            // where the reference module's `glob.glob` reads it from too.
            None => match std::env::current_dir() {
                Ok(dir) => dir,
                Err(_) => return false,
            },
        }
    };
    // Split on the separator rather than walking `Path::components`, which normalises `.` and
    // `..` away: `../shared/.migrated` has to reach the parent, not the directory itself.
    let parts: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    walk(&start, &parts, pattern.ends_with('/'))
}

fn walk(at: &Path, parts: &[&str], needs_dir: bool) -> bool {
    let Some((head, rest)) = parts.split_first() else {
        // A dangling symlink is a match: `glob.glob` reports one, through `os.path.lexists` on
        // a pattern with no metacharacter and through the directory entry itself otherwise. A
        // pattern written with a trailing separator only matches a directory.
        return if needs_dir {
            at.is_dir()
        } else {
            at.symlink_metadata().is_ok()
        };
    };
    if !head.contains(['*', '?', '[']) {
        return walk(&at.join(head), rest, needs_dir);
    }
    // A directory that cannot be listed holds nothing we can match, which is what `glob.glob`
    // reports for it too: no entry rather than an error.
    let Ok(entries) = std::fs::read_dir(at) else {
        return false;
    };
    // A metacharacter never matches a leading dot, which is what `glob.glob` does: a hidden
    // name is reachable only by a pattern that spells the dot out.
    let hidden_too = head.starts_with('.');
    entries.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        (hidden_too || !name.starts_with('.'))
            && matches_one(&name, head)
            && walk(&e.path(), rest, needs_dir)
    })
}

/// One component against one pattern. Backtracking on `*` is written out rather than recursive
/// so a pattern of many stars cannot blow the stack on a long name.
fn matches_one(name: &str, pattern: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while i < n.len() {
        // How far the pattern advances when this one name character is consumed, or `None`
        // when it cannot be: one place to backtrack into the last `*` from.
        let step = match p.get(j) {
            Some('*') => {
                star = Some(j);
                mark = i;
                j += 1;
                continue;
            }
            Some('?') => Some(1),
            Some('[') => match class(&p[j..], n[i]) {
                Some((len, true)) => Some(len),
                // No closing `]`: `[` is a literal character, matching `fnmatch`. A class that
                // is well formed but did not match is *not* re-read as a literal.
                None if n[i] == '[' => Some(1),
                _ => None,
            },
            Some(c) if *c == n[i] => Some(1),
            _ => None,
        };
        match step {
            Some(len) => {
                i += 1;
                j += len;
            }
            None => match star {
                Some(s) => {
                    j = s + 1;
                    mark += 1;
                    i = mark;
                }
                None => return false,
            },
        }
    }
    p[j..].iter().all(|c| *c == '*')
}

/// A `[...]` class starting at `p[0]`: its length in characters, and whether `c` is in it.
/// `None` means this is not a class at all — a `[` with no closing `]` is a literal, which is
/// what `fnmatch` does with it — and that is a different answer from a class `c` is not in.
/// Only `!` negates; `fnmatch` escapes a leading `^`, so `[^a]` is the class `{^, a}`. A `]`
/// straight after the `[` (or after the `!`) is a member rather than the terminator.
fn class(p: &[char], c: char) -> Option<(usize, bool)> {
    let mut end = 1;
    let negated = p.get(end) == Some(&'!');
    if negated {
        end += 1;
    }
    let body_start = end;
    if p.get(end) == Some(&']') {
        end += 1;
    }
    while end < p.len() && p[end] != ']' {
        end += 1;
    }
    if end == p.len() {
        return None;
    }
    let body = &p[body_start..end];
    let mut hit = false;
    let mut k = 0;
    while k < body.len() {
        if k + 2 < body.len() && body[k + 1] == '-' {
            if body[k] <= c && c <= body[k + 2] {
                hit = true;
            }
            k += 3;
        } else {
            if body[k] == c {
                hit = true;
            }
            k += 1;
        }
    }
    Some((end + 1, hit != negated))
}

#[cfg(test)]
mod tests;
