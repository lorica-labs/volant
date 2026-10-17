// SPDX-License-Identifier: GPL-3.0-or-later
//! The program as `ssh` runs it for `SSH_ASKPASS`: the password on stdout for a password
//! prompt, nothing and a failure for any other question.

use std::process::Command;

#[test]
fn the_binary_answers_as_an_askpass() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pw");
    std::fs::write(&file, "sek ret ").unwrap();
    let ask = |prompt: &str| {
        Command::new(env!("CARGO_BIN_EXE_volant"))
            .arg(prompt)
            .env("VOLANT_ASKPASS_FILE", &file)
            .output()
            .unwrap()
    };
    let answered = ask("user@host's password: ");
    assert!(answered.status.success());
    assert_eq!(answered.stdout, b"sek ret \n");
    let refused = ask("Are you sure you want to continue connecting (yes/no)? ");
    assert!(!refused.status.success());
    assert!(refused.stdout.is_empty());
}
