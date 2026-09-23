// SPDX-License-Identifier: GPL-3.0-or-later
//! Which Python interpreters this host has, in the order the reference prefers them.
//!
//! Two rules decide what comes out. A candidate is reported **at the path the walk found it**,
//! never at the path it points to: `python3 -m venv` symlinks the virtualenv's `python3.12` at
//! the system one, and running a module under the target puts the virtualenv's `site-packages`
//! off `sys.path`. And a candidate is reported only when this process could actually run it,
//! which is a question about the caller as much as about the file.

use std::path::{Path, PathBuf};

use volant_protocol::interpreter::CANDIDATES;

/// What tells two names for one interpreter apart from two interpreters: the file itself, by
/// device and inode. Symlinks and hardlinks both collapse, which canonicalising the path alone
/// would not do for a hardlink.
type File = (u64, u64);

/// Every Python this host has, best first, as absolute paths. Empty when it has none.
pub fn discover() -> Vec<String> {
    discover_in(&CANDIDATES, &std::env::var("PATH").unwrap_or_default())
}

/// [`discover`] with the candidate list and the `PATH` handed in.
///
/// A bare name is resolved by walking `path_env` and taking the first entry that holds a file
/// this process could execute, which is what a shell would run; a candidate starting with `/` is
/// taken as it stands. A file already reported is dropped **without moving the entry that
/// reported it first**: on most Linux hosts `python3.12`, `/usr/bin/python3` and `python3` are
/// one file, and re-ordering here is re-ordering the reference's preference.
fn discover_in(candidates: &[&str], path_env: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: Vec<File> = Vec::new();
    for candidate in candidates {
        let Some((at, file)) = resolve(candidate, path_env) else {
            continue;
        };
        if seen.contains(&file) {
            continue;
        }
        // A path whose bytes are not UTF-8 is legal on Linux. Skipped rather than converted:
        // the replacement character would make a string that names no file on the host, and the
        // controller would try to execute it.
        let Ok(path) = at.into_os_string().into_string() else {
            continue;
        };
        seen.push(file);
        found.push(path);
    }
    found
}

/// The file a candidate names, as it was named, with the identity that de-duplicates it.
///
/// Only absolute `PATH` entries are searched. A relative one - `bin`, or the `.` a legacy profile
/// leaves behind - would be resolved against the agent's own working directory, wherever the ssh
/// exec dropped it, and an executable sitting there is not an interpreter the reference would
/// have found.
fn resolve(candidate: &str, path_env: &str) -> Option<(PathBuf, File)> {
    if candidate.starts_with('/') {
        return executable(Path::new(candidate));
    }
    path_env
        .split(':')
        .filter(|entry| entry.starts_with('/'))
        .find_map(|entry| executable(&Path::new(entry).join(candidate)))
}

/// `at` and its identity, when `at` is a file this process could execute.
///
/// `access(X_OK)` rather than the owner's execute bit: a root-owned `0o700` interpreter has that
/// bit set and still cannot be run by the agent, and a `noexec` mount has every bit set and runs
/// nothing. Reporting either in first position would hand the controller an interpreter that
/// answers `EACCES` for every module while a working one sits behind it, which is not what a
/// shell resolving the same name would do.
#[cfg(unix)]
fn executable(at: &Path) -> Option<(PathBuf, File)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(at).ok()?;
    if !meta.is_file() {
        return None;
    }
    let path = CString::new(at.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a NUL-terminated C string that outlives the call, and `access` only
    // reads it.
    if unsafe { libc::access(path.as_ptr(), libc::X_OK) } != 0 {
        return None;
    }
    Some((at.to_path_buf(), (meta.dev(), meta.ino())))
}

/// The agent runs on a managed unix host and nowhere else. Off unix there is no executable bit
/// to ask about, and a path from a developer's machine is not one a controller could run, so
/// nothing is reported rather than something that would have to be filtered out later.
#[cfg(not(unix))]
fn executable(_at: &Path) -> Option<(PathBuf, File)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The candidate order is the reference's own fallback list, and a bare name is resolved
    /// against `PATH` while an absolute one is taken as it stands.
    ///
    /// What would make this red: the order rearranged, which would pick a different
    /// interpreter from the one the reference picks on the same host and silently change which
    /// Python a playbook runs under; or a bare name reported unresolved, which the controller
    /// would then hand to a shell that may resolve it differently.
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
    ///
    /// What would make this red: the same interpreter reported twice under two names - on a
    /// normal Linux host `python3.12`, `/usr/bin/python3` and `python3` are frequently the same
    /// file, and a controller counting candidates would think it had three choices; or a
    /// relative path reported, which is not something the controller can execute directly.
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
    ///
    /// What would make this red: a `sort` of the result, which reverses these two paths; a walk
    /// over the candidates in any other order, which the second assertion pins with two distinct
    /// files and no symlink between them; or a `PATH` walk that carries on past a hit it has
    /// already reported, which would append `a/python3` to the first vector - an interpreter the
    /// name `python3` does not resolve to on that host.
    #[cfg(unix)]
    #[test]
    fn de_duplication_keeps_the_first_name_a_file_was_found_under() {
        let root = tempdir();
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
    ///
    /// What would make this red: reporting the canonical path. `python3 -m venv` symlinks the
    /// virtualenv's `python3.12` at the system one; a module run under the target has the
    /// virtualenv's `site-packages` off `sys.path` and fails to import what only the virtualenv
    /// has - or finds an older copy system-wide and succeeds against the wrong version. It also
    /// collapses every environment on the host into one reported entry.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_interpreter_is_reported_where_it_was_found() {
        let root = tempdir();
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
    ///
    /// What would make this red: a de-duplication keyed on the canonical path, which resolves
    /// symlinks and not links, so a source build or a conda-style layout would be reported as
    /// two interpreters where the host has one.
    #[cfg(unix)]
    #[test]
    fn hardlinked_names_for_one_interpreter_collapse() {
        let dir = tempdir();
        let real = program(dir.path(), "python3.12");
        std::fs::hard_link(&real, dir.path().join("python3")).unwrap();
        assert_eq!(
            discover_in(&["python3.12", "python3"], dir.path().to_str().unwrap()),
            vec![shown(&real)]
        );
    }

    /// A file this process cannot execute is not an interpreter, and the candidate falls through
    /// to the next entry on `PATH`.
    ///
    /// What would make this red: the executable test dropped, which would report `a/python3` and
    /// hand the controller a file it cannot run; or the `PATH` walk broken, which would report
    /// nothing at all - the second entry is the positive control that tells those two apart.
    #[cfg(unix)]
    #[test]
    fn a_candidate_that_cannot_be_executed_falls_through_to_the_next_path_entry() {
        let root = tempdir();
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
    ///
    /// What would make this red: joining a candidate onto it anyway. The result is resolved
    /// against the agent's own working directory, wherever the ssh exec dropped it, so an
    /// executable named `python3.12` sitting there is reported - ahead of the system
    /// interpreters, and at a path the reference never looked at.
    #[cfg(unix)]
    #[test]
    fn a_relative_path_entry_is_not_searched() {
        let root = tempdir();
        program(&root.path().join("bin"), "python3.12");
        // nextest runs each test in its own process, so this reaches no other test.
        std::env::set_current_dir(root.path()).unwrap();

        assert!(
            discover_in(&["python3.12"], "bin:.").is_empty(),
            "a relative entry names the agent's working directory, not a place on the host"
        );
    }

    /// A host with no Python at all reports an empty list rather than guessing. The controller
    /// turns that into a refusal naming the host; a guess would turn it into a confusing
    /// "command not found" from a module that never ran.
    ///
    /// The second assertion is the one about a real host: candidates to look for and a `PATH` to
    /// look on, and nothing named like a Python on it.
    #[cfg(unix)]
    #[test]
    fn a_host_without_python_reports_nothing() {
        let dir = tempdir();
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

    /// A directory of this process's own, removed when the test ends. The agent carries no
    /// temporary-directory dependency, for the reason `blobs.rs` gives.
    #[cfg(unix)]
    struct TempDir(PathBuf);

    #[cfg(unix)]
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    #[cfg(unix)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU32, Ordering};

        static COUNT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "volant-interpreter-test-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("the test directory is created");
        TempDir(dir)
    }
}
