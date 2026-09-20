// SPDX-License-Identifier: GPL-3.0-or-later
//! Whether at least one path matches a pattern, the way `glob.glob` answers it: `*`, `?` and
//! `[...]` match inside one path component and never across a `/`. `**` is not special, which
//! is what `glob.glob` does without `recursive=True`.

use std::path::{Component, Path, PathBuf};

/// Whether anything matches `pattern`, resolved against `base` when it is relative.
pub(crate) fn matches_any(base: Option<&Path>, pattern: &str) -> bool {
    let path = Path::new(pattern);
    let start = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        base.map(Path::to_path_buf).unwrap_or_default()
    };
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    walk(&start, &parts)
}

fn walk(at: &Path, parts: &[String]) -> bool {
    let Some((head, rest)) = parts.split_first() else {
        return at.exists();
    };
    if !head.contains(['*', '?', '[']) {
        return walk(&at.join(head), rest);
    }
    // A directory that cannot be listed holds nothing we can match, which is what `glob.glob`
    // reports for it too: no entry rather than an error.
    let Ok(entries) = std::fs::read_dir(at) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| matches_one(&e.file_name().to_string_lossy(), head) && walk(&e.path(), rest))
}

/// One component against one pattern. Backtracking on `*` is written out rather than recursive
/// so a pattern of many stars cannot blow the stack on a long name.
fn matches_one(name: &str, pattern: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while i < n.len() {
        match p.get(j) {
            Some('*') => {
                star = Some(j);
                mark = i;
                j += 1;
            }
            Some('?') => {
                i += 1;
                j += 1;
            }
            Some('[') => match class(&p[j..], n[i]) {
                Some(len) => {
                    i += 1;
                    j += len;
                }
                // No closing `]`: `[` is a literal character, matching `fnmatch`.
                None if n[i] == '[' => {
                    i += 1;
                    j += 1;
                }
                None => match star {
                    Some(s) => {
                        j = s + 1;
                        mark += 1;
                        i = mark;
                    }
                    None => return false,
                },
            },
            Some(c) if *c == n[i] => {
                i += 1;
                j += 1;
            }
            _ => match star {
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

/// A `[...]` class starting at `p[0]`, and whether `c` is in it. Returns the class's length in
/// characters when it matches, so the caller can step over it. A `[` with no `]` is a literal,
/// which is what `fnmatch` does with it.
fn class(p: &[char], c: char) -> Option<usize> {
    let end = p.iter().position(|x| *x == ']')?;
    if end == 1 {
        return None;
    }
    let (negated, body) = match p[1] {
        '!' | '^' => (true, &p[2..end]),
        _ => (false, &p[1..end]),
    };
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
    (hit != negated).then_some(end + 1)
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

    #[test]
    fn question_mark_is_exactly_one_character() {
        let dir = tempdir();
        std::fs::write(dir.join("ab"), b"").unwrap();
        assert!(matches_any(Some(&dir), "a?"));
        assert!(!matches_any(Some(&dir), "a??"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
        assert!(!matches_any(Some(&dir), "locked/marker"));
        std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A pattern with no metacharacter reduces to `exists`.
    #[test]
    fn no_metacharacter_reduces_to_exists() {
        let dir = tempdir();
        assert!(!matches_any(Some(&dir), "marker"));
        std::fs::write(dir.join("marker"), b"").unwrap();
        assert!(matches_any(Some(&dir), "marker"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_absolute_pattern_ignores_base() {
        let dir = tempdir();
        std::fs::write(dir.join("marker"), b"").unwrap();
        let pattern = format!("{}/marker", dir.display());
        assert!(matches_any(Some(Path::new("/does/not/exist")), &pattern));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
