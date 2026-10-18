// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

/// A reader that panics says so on the channel rather than dropping its sender, which the batch
/// would read as a cancel nobody sent.
#[test]
fn a_panicking_stdin_reader_reports_an_error() {
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || guard_reader(&tx, |_| panic!("reader bug")));
    let _ = reader.join();
    match rx.recv() {
        Ok(Err(err)) => assert_eq!(err.to_string(), "the agent's stdin reader panicked"),
        Ok(Ok(_)) => panic!("a message instead of the error"),
        Err(_) => panic!("the reader ended without a word"),
    }
}

/// A host without procfs still answers the probe with the hash of the file it was started from,
/// rather than failing it and leaving the host unreachable over an agent that runs.
#[test]
fn without_procfs_the_hash_is_of_the_absolute_path_the_agent_was_started_by() {
    let dir = std::env::temp_dir().join(format!("volant-self-hash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("volant-agent");
    std::fs::write(&file, b"the agent's own bytes").unwrap();
    let hash = hash_of_self(&dir.join("no-proc-exe"), Some(file.into_os_string()));
    assert_eq!(
        hash.unwrap(),
        blake3::hash(b"the agent's own bytes").to_hex().to_string()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
