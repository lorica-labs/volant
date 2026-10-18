// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// A password prompt gets the file's text and a newline; a question that is not for a
/// password gets nothing.
#[rstest::rstest]
#[case::password("user@host's password: ", Some("sek ret \n"))]
#[case::keyboard_interactive("Password: ", Some("sek ret \n"))]
#[case::host_key("Are you sure you want to continue connecting (yes/no)? ", None)]
#[case::passphrase("Enter passphrase for key '/k': ", None)]
#[case::passphrase_of_a_password_key("Enter passphrase for key '/x/password.pem': ", None)]
#[case::host_key_of_a_password_host(
    "The authenticity of host 'password-db' can't be established. Are you sure you want to continue connecting (yes/no)? ",
    None
)]
#[case::user_password(
    "(user@host) Password: ",
    Some(
        "sek ret 
"
    )
)]
fn the_askpass_answers_password_prompts_only(#[case] prompt: &str, #[case] want: Option<&str>) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pw");
    std::fs::write(&file, "sek ret ").unwrap();
    assert_eq!(
        reply(&file, prompt),
        want.map(|text| text.as_bytes().to_vec())
    );
}

#[test]
fn a_missing_file_gives_no_answer() {
    assert_eq!(
        reply(Path::new("/nonexistent/volant/pw"), "password: "),
        None
    );
}
