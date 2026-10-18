// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
#[cfg(unix)]
use crate::testing::scratch;

/// The candidate order is the reference's own fallback list, and a bare name is resolved
/// against `PATH` while an absolute one is taken as it stands.
#[test]
fn the_candidate_order_is_the_reference_s_own() {
    assert_eq!(
        CANDIDATES,
        [
            "python3.13",
            "python3.12",
            "python3.11",
            "python3.10",
            "python3.9",
            "python3.8",
            "/usr/bin/python3",
            "python3",
        ]
    );
}

/// Discovery reports absolute paths only, every one of them executable, with no duplicates.
#[cfg(unix)]
#[test]
fn discovery_reports_absolute_executable_paths_without_duplicates() {
    let found = discover();
    for path in &found {
        assert!(path.starts_with('/'), "not absolute: {path}");
        let meta = std::fs::metadata(path).expect("reported an interpreter that is not there");
        assert!(meta.is_file(), "not a file: {path}");
    }
    let mut sorted = found.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        found.len(),
        "a duplicate was reported: {found:?}"
    );
}

/// De-duplication keeps the first occurrence where it was, the candidate order decides which
/// interpreter comes first, and a candidate stops at the first `PATH` entry that answers it.
#[cfg(unix)]
#[test]
fn de_duplication_keeps_the_first_name_a_file_was_found_under() {
    let root = scratch();
    // `b` before `a` on the path, and the better interpreter in `b`: sorting the answer
    // would put `a`'s ahead of it, so the assertion below can tell the two apart.
    let first = root.path().join("b");
    let second = root.path().join("a");
    let best = program(&first, "python3.12");
    link(&best, &first.join("python3.11"));
    link(&best, &first.join("python3"));
    let other = program(&second, "python3.9");
    // A second, distinct `python3`, behind the one in `b` that is already a duplicate.
    program(&second, "python3");
    let path_env = format!("{}:{}", first.display(), second.display());

    assert_eq!(
        discover_in(
            &[
                "python3.12",
                "python3.11",
                "python3.10",
                "python3.9",
                "python3"
            ],
            &path_env,
        ),
        vec![shown(&best), shown(&other)],
        "one file is reported once, under the first candidate that found it, and the walk \
             stops at the first entry that answers a candidate"
    );
    assert_eq!(
        discover_in(&["python3.9", "python3.12"], &path_env),
        vec![shown(&other), shown(&best)],
        "the candidate order, not the path order, decides which comes first"
    );
}

/// An interpreter is reported where it was found, not where it points.
#[cfg(unix)]
#[test]
fn a_symlinked_interpreter_is_reported_where_it_was_found() {
    let root = scratch();
    let system = root.path().join("usr");
    let venv = root.path().join("venv");
    let real = program(&system, "python3.12");
    std::fs::create_dir_all(&venv).unwrap();
    link(&real, &venv.join("python3.12"));
    let path_env = format!("{}:{}", venv.display(), system.display());

    assert_eq!(
        discover_in(&["python3.12"], &path_env),
        vec![shown(&venv.join("python3.12"))],
        "the virtualenv's own path, not the interpreter it points at"
    );
    link(&real, &venv.join("python3"));
    assert_eq!(
        discover_in(&["python3.12", "python3"], &path_env).len(),
        1,
        "one file is one interpreter, whatever names lead to it"
    );
}

/// Two hardlinks to one interpreter are one interpreter: what de-duplicates is the file, not
/// the path that reached it.
#[cfg(unix)]
#[test]
fn hardlinked_names_for_one_interpreter_collapse() {
    let dir = scratch();
    let real = program(dir.path(), "python3.12");
    std::fs::hard_link(&real, dir.path().join("python3")).unwrap();
    assert_eq!(
        discover_in(&["python3.12", "python3"], dir.path().to_str().unwrap()),
        vec![shown(&real)]
    );
}

/// A file this process cannot execute is not an interpreter, and the candidate falls through to
/// the next entry on `PATH`.
#[cfg(unix)]
#[test]
fn a_candidate_that_cannot_be_executed_falls_through_to_the_next_path_entry() {
    let root = scratch();
    let first = root.path().join("a");
    let second = root.path().join("b");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::write(first.join("python3"), "not a program").unwrap();
    let runnable = program(&second, "python3");

    assert_eq!(
        discover_in(
            &["python3"],
            &format!("{}:{}", first.display(), second.display())
        ),
        vec![shown(&runnable)]
    );
}

/// A relative `PATH` entry is not searched.
#[cfg(unix)]
#[test]
fn a_relative_path_entry_is_not_searched() {
    let root = scratch();
    program(&root.path().join("bin"), "python3.12");
    // nextest runs each test in its own process, so this reaches no other test.
    std::env::set_current_dir(root.path()).unwrap();

    assert!(
        discover_in(&["python3.12"], "bin:.").is_empty(),
        "a relative entry names the agent's working directory, not a place on the host"
    );
}

/// A host with no Python at all reports an empty list rather than guessing.
#[cfg(unix)]
#[test]
fn a_host_without_python_reports_nothing() {
    let dir = scratch();
    assert!(discover_in(&[], "").is_empty());
    assert!(
        discover_in(&["python3", "python3.12"], dir.path().to_str().unwrap()).is_empty(),
        "an empty directory holds no interpreter"
    );
}

/// An executable file named `name` in `dir`, which is created if it is not there.
#[cfg(unix)]
fn program(dir: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir).unwrap();
    let at = dir.join(name);
    std::fs::write(&at, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
    at
}

#[cfg(unix)]
fn link(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

/// The path as discovery reports it: the one the walk found, unresolved.
#[cfg(unix)]
fn shown(at: &Path) -> String {
    at.to_string_lossy().into_owned()
}
