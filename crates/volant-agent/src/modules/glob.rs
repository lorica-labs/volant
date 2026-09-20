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

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "volant-glob-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What would make this red: a `*` implemented with a plain wildcard scan across the whole
    /// path string instead of one path component at a time.
    #[test]
    fn star_does_not_cross_a_slash() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("marker"), b"").unwrap();
        // `*` alone must not reach into `sub/marker` from the top.
        assert!(!matches_any(Some(&dir), "*marker"));
        assert!(matches_any(Some(&dir), "s*/marker"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What would make this red: `?` implemented as "zero or one character", or as a second
    /// spelling of `*`, either of which would let `a??` match the two-character `ab`.
    #[test]
    fn question_mark_is_exactly_one_character() {
        let dir = tempdir();
        std::fs::write(dir.join("ab"), b"").unwrap();
        assert!(matches_any(Some(&dir), "a?"));
        assert!(!matches_any(Some(&dir), "a??"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What would make this red: a class parser that compares the body character by character
    /// without reading `-` as a range, so `[a-c]` would be the three-member set `{a, -, c}`
    /// and would not match `b`.
    #[test]
    fn a_bracket_class_matches_a_range_and_its_negation() {
        let dir = tempdir();
        std::fs::write(dir.join("b"), b"").unwrap();
        assert!(matches_any(Some(&dir), "[a-c]"));
        assert!(!matches_any(Some(&dir), "[!a-c]"));
        std::fs::write(dir.join("z"), b"").unwrap();
        assert!(matches_any(Some(&dir), "[!a-c]"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `fnmatch.translate` escapes a leading `^`, so only `!` negates a class and `[^a]` is
    /// the two-member set `{^, a}`.
    ///
    /// What would make this red: reading `^` as a negation the way a shell does, which would
    /// invert both answers below and let `creates: "[^0-9]*"` run a command the reference
    /// skips.
    #[test]
    fn a_caret_in_a_class_is_a_member_not_a_negation() {
        let dir = tempdir();
        std::fs::write(dir.join("a"), b"").unwrap();
        assert!(matches_any(Some(&dir), "[^a]"));
        assert!(!matches_any(Some(&dir), "[^b]"));
        std::fs::write(dir.join("^"), b"").unwrap();
        assert!(matches_any(Some(&dir), "[^b]"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Three shapes `fnmatch` reads through its own bracket scan: a class that is well formed
    /// but does not match is a miss, not a literal `[`; a `]` straight after the `[` is a
    /// member; and a `[!]` has no closing bracket left, so it is literal.
    ///
    /// What would make this red: a `class` that answers `None` for both "not a class" and
    /// "class did not match", which makes the caller re-read a well-formed class as a literal
    /// `[` and makes `[!]` an empty negated body that matches any single character.
    #[test]
    fn a_class_that_did_not_match_is_not_re_read_as_a_literal() {
        let dir = tempdir();
        std::fs::write(dir.join("[ab]"), b"").unwrap();
        std::fs::write(dir.join("]"), b"").unwrap();
        std::fs::write(dir.join("[!]"), b"").unwrap();
        assert!(!matches_any(Some(&dir), "[ab]"));
        assert!(matches_any(Some(&dir), "[]]"));
        assert!(matches_any(Some(&dir), "[!]"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `glob.glob` never lets a metacharacter reach a name that starts with a dot; only a
    /// pattern spelling the dot out does.
    ///
    /// What would make this red: matching directory entries without filtering hidden names,
    /// which makes `creates: "*"` read a directory holding only `.keep` as populated and skip
    /// a command the reference runs.
    #[test]
    fn a_metacharacter_does_not_match_a_hidden_name() {
        let dir = tempdir();
        std::fs::write(dir.join(".keep"), b"").unwrap();
        assert!(!matches_any(Some(&dir), "*"));
        assert!(!matches_any(Some(&dir), "?keep"));
        assert!(!matches_any(Some(&dir), "[.]keep"));
        assert!(matches_any(Some(&dir), ".k*"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `glob.glob("data/")` returns nothing when `data` is a regular file.
    ///
    /// What would make this red: dropping the trailing separator — which `Path::components`
    /// does — and ending the walk on plain existence, so `creates: "output/"` would be
    /// satisfied by a stray regular file named `output`.
    #[test]
    fn a_trailing_separator_requires_a_directory() {
        let dir = tempdir();
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
    ///
    /// What would make this red: ending the walk on `Path::exists`, which follows the link and
    /// answers `false`, so `creates: .initialized` pointing at a removed target would re-run
    /// an initialiser the reference skips.
    #[test]
    #[cfg(unix)]
    fn a_dangling_symlink_matches() {
        let dir = tempdir();
        std::os::unix::fs::symlink("/definitely/not/here", dir.join(".initialized")).unwrap();
        assert!(matches_any(Some(&dir), ".initialized"));
        assert!(matches_any(Some(&dir), ".init*"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `..` reaches the parent directory, the way `glob.glob("../x-1")` does.
    ///
    /// What would make this red: building the component list from `Path::components`, which
    /// normalises `.` and `..` away, so a guard written `creates: "../shared/.migrated"` under
    /// a `chdir` would read the `chdir` itself and let a migration run on every deploy.
    #[test]
    fn a_parent_component_is_not_normalised_away() {
        let dir = tempdir();
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
    ///
    /// What would make this red: an empty component list ending the walk on the base's own
    /// existence, which is `true` for every `chdir` that exists.
    #[test]
    fn an_empty_pattern_matches_nothing() {
        let dir = tempdir();
        assert!(!matches_any(Some(&dir), ""));
        assert!(!matches_any(None, ""));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Cell (n): with no base at all, a pattern resolves against the process's own directory —
    /// not against the empty path. `cargo nextest` gives every test its own process, so moving
    /// that directory here cannot reach another test.
    ///
    /// What would make this red: seeding the walk with `PathBuf::default()`. A literal
    /// component survives it, because `Path::new("").join("x-1")` is `x-1`, but the first
    /// component holding a metacharacter goes to `read_dir("")`, which is `ENOENT` on POSIX —
    /// so `creates: ".initialized-*"` with no `chdir` never matches and the command runs
    /// forever.
    #[test]
    fn a_pattern_with_no_base_reads_the_process_directory() {
        let dir = tempdir();
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

    /// What would make this red: treating an unterminated `[` as the start of a class instead
    /// of a literal character, which is what `fnmatch` does with it.
    #[test]
    fn an_unterminated_bracket_is_literal() {
        let dir = tempdir();
        std::fs::write(dir.join("[a"), b"").unwrap();
        assert!(matches_any(Some(&dir), "[a"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What would make this red: propagating the `read_dir` error instead of treating it as an
    /// empty listing, which is what `glob.glob` does for a directory it cannot read.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_directory_matches_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
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
    ///
    /// What would make this red: listing the base directory for every component instead of
    /// joining a component that holds no metacharacter, which turns a guard on a path whose
    /// parent cannot be listed into a miss.
    #[test]
    fn no_metacharacter_reduces_to_exists() {
        let dir = tempdir();
        assert!(!matches_any(Some(&dir), "marker"));
        std::fs::write(dir.join("marker"), b"").unwrap();
        assert!(matches_any(Some(&dir), "marker"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Cell (m): an absolute pattern is expanded from `/`, whatever `chdir` a sibling task set.
    ///
    /// What would make this red: joining the pattern onto the base unconditionally, which sends
    /// an absolute `creates` under the `chdir` and makes it miss.
    #[test]
    fn an_absolute_pattern_ignores_base() {
        let dir = tempdir();
        std::fs::write(dir.join("marker"), b"").unwrap();
        let pattern = format!("{}/marker", dir.display());
        assert!(matches_any(Some(Path::new("/does/not/exist")), &pattern));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
