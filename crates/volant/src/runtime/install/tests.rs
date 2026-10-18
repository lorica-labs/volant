// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::testing::scratch;

use rstest::rstest;

/// A lock is taken when it is free, or held by a dead pid or by nothing readable; a live holder
/// keeps it.
#[rstest]
#[case::free(None, true)]
#[case::dead_pid(Some("2147483000\n"), true)]
#[case::negative_pid(Some("-1\n"), true)]
#[case::no_pid(Some("garbage"), true)]
#[case::live_pid(Some("1\n"), false)]
fn a_lock_is_taken_over_only_from_a_dead_holder(#[case] held: Option<&str>, #[case] taken: bool) {
    let dir = scratch();
    let path = dir.path().join(".id.lock");
    if let Some(held) = held {
        fs::write(&path, held).unwrap();
    }
    match Lock::acquire(&path, Duration::ZERO) {
        Ok(lock) => {
            assert!(taken, "a live holder's lock was taken");
            let pid = std::process::id().to_string();
            assert_eq!(fs::read_to_string(&path).unwrap().trim(), pid);
            drop(lock);
            assert!(!path.exists(), "the lock outlived its holder");
        }
        Err(e) => {
            assert!(!taken, "{e}");
            assert!(e.contains("pid 1") && e.contains("still installing"), "{e}");
            assert_eq!(fs::read_to_string(&path).unwrap(), held.unwrap());
        }
    }
}

/// What a killed install left is deleted before the next one; other runtimes' files stay.
#[test]
fn partial_installs_of_the_runtime_are_swept() {
    let dir = scratch();
    for name in [
        ".id.partial-a1",
        ".id.partial-b2/runtime",
        ".other.partial-c3",
        "id-old",
    ] {
        fs::create_dir_all(dir.path().join(name)).unwrap();
    }
    sweep_partials(dir.path(), "id");
    let mut left: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, [".other.partial-c3", "id-old"]);
}

/// A directory counts as a runtime only with its record and its interpreter.
#[rstest]
#[case::whole(true, true, true)]
#[case::no_record(false, true, false)]
#[case::no_python(true, false, false)]
fn only_a_whole_runtime_is_installed(
    #[case] record: bool,
    #[case] python: bool,
    #[case] counted: bool,
) {
    let dir = scratch();
    let runtime = dir.path().join("id");
    fs::create_dir_all(runtime.join("python/bin")).unwrap();
    if record {
        fs::write(runtime.join(RECORD), "{}").unwrap();
    }
    if python {
        fs::write(runtime.join("python/bin/python3"), "").unwrap();
    }
    assert_eq!(installed(dir.path(), "id").is_some(), counted);
    assert_eq!(list(dir.path()).len(), usize::from(counted));
}

#[rstest]
#[case::empty("")]
#[case::hidden(".id.lock")]
#[case::path("../elsewhere")]
fn remove_refuses_what_is_not_a_runtime_name(#[case] id: &str) {
    let dir = scratch();
    let e = remove(dir.path(), id).unwrap_err();
    assert!(e.contains("is not the name of a runtime"), "{e}");
}

/// The real install, from the pinned URLs: run with `--run-ignored only` on a machine online.
#[test]
#[ignore = "needs network access to GitHub and PyPI"]
fn the_pinned_runtime_installs_and_imports_ansible_core() {
    let dir = scratch();
    let runtimes = dir.path().join("runtime");
    let manifest = Manifest::pinned().unwrap();
    let done = install(&runtimes, &manifest).unwrap();
    assert_eq!(done, installed(&runtimes, &manifest.id()).unwrap());
    probe(&done.python, &manifest.ansible_core).unwrap();
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(done.dir.join(RECORD)).unwrap()).unwrap();
    assert_eq!(record["ansible_core"], "2.19.12");
    assert_eq!(list(&runtimes), [manifest.id()]);
    assert_eq!(install(&runtimes, &manifest).unwrap(), done);
    assert!(remove(&runtimes, &manifest.id()).unwrap());
    assert!(list(&runtimes).is_empty());
}
