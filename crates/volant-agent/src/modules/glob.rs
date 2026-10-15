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
mod tests {
    use super::*;
    use crate::testing::scratch;

    /// `*` matches within one path component and never crosses a slash.
    #[test]
    fn star_does_not_cross_a_slash() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("marker"), b"").unwrap();
        // `*` alone must not reach into `sub/marker` from the top.
        assert!(!matches_any(Some(&dir), "*marker"));
        assert!(matches_any(Some(&dir), "s*/marker"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `?` matches exactly one character, so `a??` does not match the two-character `ab`.
    #[test]
    fn question_mark_is_exactly_one_character() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::write(dir.join("ab"), b"").unwrap();
        assert!(matches_any(Some(&dir), "a?"));
        assert!(!matches_any(Some(&dir), "a??"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A bracket class reads `-` as a range and `!` or `^` as a negation, so `[a-c]` matches `b`.
    #[test]
    fn a_bracket_class_matches_a_range_and_its_negation() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::write(dir.join("b"), b"").unwrap();
        assert!(matches_any(Some(&dir), "[a-c]"));
        assert!(!matches_any(Some(&dir), "[!a-c]"));
        std::fs::write(dir.join("z"), b"").unwrap();
        assert!(matches_any(Some(&dir), "[!a-c]"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `fnmatch.translate` escapes a leading `^`, so only `!` negates a class and `[^a]` is the
    /// two-member set `{^, a}`.
    #[test]
    fn a_caret_in_a_class_is_a_member_not_a_negation() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::write(dir.join("a"), b"").unwrap();
        assert!(matches_any(Some(&dir), "[^a]"));
        assert!(!matches_any(Some(&dir), "[^b]"));
        std::fs::write(dir.join("^"), b"").unwrap();
        assert!(matches_any(Some(&dir), "[^b]"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A well-formed class that does not match is a miss, a leading `]` is a member, and `[!]` is
    /// literal.
    #[test]
    fn a_class_that_did_not_match_is_not_re_read_as_a_literal() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::write(dir.join("[ab]"), b"").unwrap();
        std::fs::write(dir.join("]"), b"").unwrap();
        std::fs::write(dir.join("[!]"), b"").unwrap();
        assert!(!matches_any(Some(&dir), "[ab]"));
        assert!(matches_any(Some(&dir), "[]]"));
        assert!(matches_any(Some(&dir), "[!]"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `glob.glob` never lets a metacharacter reach a name that starts with a dot; only a pattern
    /// spelling the dot out does.
    #[test]
    fn a_metacharacter_does_not_match_a_hidden_name() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::write(dir.join(".keep"), b"").unwrap();
        assert!(!matches_any(Some(&dir), "*"));
        assert!(!matches_any(Some(&dir), "?keep"));
        assert!(!matches_any(Some(&dir), "[.]keep"));
        assert!(matches_any(Some(&dir), ".k*"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `glob.glob("data/")` returns nothing when `data` is a regular file.
    #[test]
    fn a_trailing_separator_requires_a_directory() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::write(dir.join("data"), b"").unwrap();
        std::fs::create_dir_all(dir.join("deep")).unwrap();
        assert!(!matches_any(Some(&dir), "data/"));
        assert!(matches_any(Some(&dir), "data"));
        assert!(matches_any(Some(&dir), "deep/"));
        assert!(matches_any(Some(&dir), "d*p/"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `glob.glob` reports a dangling symlink: it asks `os.path.lexists` on a pattern with no
    /// metacharacter and reads directory entries otherwise, neither of which follows the link.
    #[test]
    #[cfg(unix)]
    fn a_dangling_symlink_matches() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::os::unix::fs::symlink("/definitely/not/here", dir.join(".initialized")).unwrap();
        assert!(matches_any(Some(&dir), ".initialized"));
        assert!(matches_any(Some(&dir), ".init*"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `..` reaches the parent directory, the way `glob.glob("../x-1")` does.
    #[test]
    fn a_parent_component_is_not_normalised_away() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        let deep = dir.join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(dir.join("x-1"), b"").unwrap();
        assert!(matches_any(Some(&deep), "../x-1"));
        assert!(matches_any(Some(&deep), "../x-*"));
        assert!(matches_any(Some(&dir), "deep/../x-1"));
        assert!(!matches_any(Some(&deep), "x-1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `glob.glob("")` returns nothing, and the guards themselves apply no guard at all for an
    /// empty value — a template that rendered to nothing must not decide anything.
    #[test]
    fn an_empty_pattern_matches_nothing() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        assert!(!matches_any(Some(&dir), ""));
        assert!(!matches_any(None, ""));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Cell (n): with no base at all, a pattern resolves against the process's own directory —
    /// not against the empty path.
    #[test]
    fn a_pattern_with_no_base_reads_the_process_directory() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::write(dir.join("x-1"), b"").unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let literal = matches_any(None, "x-1");
        let pattern = matches_any(None, "x-*");
        std::env::set_current_dir(previous).unwrap();
        assert!(literal, "a literal with no base");
        assert!(pattern, "a pattern with no base");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An unterminated `[` is a literal character, as `fnmatch` reads it.
    #[test]
    fn an_unterminated_bracket_is_literal() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::write(dir.join("[a"), b"").unwrap();
        assert!(matches_any(Some(&dir), "[a"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A directory that cannot be read is an empty listing, as `glob.glob` treats it.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_directory_matches_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::create_dir_all(dir.join("locked")).unwrap();
        std::fs::write(dir.join("locked").join("marker"), b"").unwrap();
        std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        assert!(!matches_any(Some(&dir), "locked/*"));
        std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A pattern with no metacharacter reduces to an existence check.
    #[test]
    fn no_metacharacter_reduces_to_exists() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        assert!(!matches_any(Some(&dir), "marker"));
        std::fs::write(dir.join("marker"), b"").unwrap();
        assert!(matches_any(Some(&dir), "marker"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Cell (m): an absolute pattern is expanded from `/`, whatever `chdir` a sibling task set.
    #[test]
    fn an_absolute_pattern_ignores_base() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::write(dir.join("marker"), b"").unwrap();
        let pattern = format!("{}/marker", dir.display());
        assert!(matches_any(Some(Path::new("/does/not/exist")), &pattern));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
