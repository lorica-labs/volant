// SPDX-License-Identifier: GPL-3.0-or-later

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
    std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(!matches_any(Some(&dir), "locked/*"));
    std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
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
