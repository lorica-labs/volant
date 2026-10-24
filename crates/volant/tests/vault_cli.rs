// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]
//! `volant vault encrypt|decrypt|view` against ansible-vault's messages, exit codes, modes and
//! files; the terminal-only messages through a pseudo-terminal.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use rstest::rstest;

#[path = "common/process.rs"]
mod process;

#[path = "common/python.rs"]
mod python;

const DEADLINE: Duration = Duration::from_secs(60);

/// A directory holding `pw` (the password), `bad` (another one), `plain`, and `vaulted`, which
/// `pw` opens. Canonical, because the messages name the real path.
struct Sandbox {
    _guard: tempfile::TempDir,
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let guard = tempfile::tempdir().expect("a directory");
        let dir = std::fs::canonicalize(guard.path()).expect("its real path");
        for (name, body) in [("pw", "pw\n"), ("bad", "wrong\n"), ("plain", "plain\n")] {
            std::fs::write(dir.join(name), body).expect("a fixture");
        }
        let sandbox = Self { _guard: guard, dir };
        std::fs::write(sandbox.dir.join("vaulted"), "secret\n").expect("a fixture");
        let out = sandbox.run(&["encrypt", "vaulted", "--vault-password-file", "pw"], None);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        sandbox
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
        command
            .arg("vault")
            .args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .env("NO_COLOR", "1")
            .env_remove("ANSIBLE_CONFIG")
            .env_remove("ANSIBLE_VAULT_PASSWORD_FILE")
            .env_remove("ANSIBLE_VAULT_IDENTITY_LIST")
            .env_remove("ANSIBLE_VAULT_ENCRYPT_IDENTITY")
            .env_remove("ANSIBLE_PAGER")
            .env_remove("ANSIBLE_EDITOR")
            .env_remove("EDITOR")
            .env_remove("ANSIBLE_HOME")
            .env("ANSIBLE_LOCAL_TEMP", self.dir.join("scratch"))
            .env_remove("PAGER");
        command
    }

    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Output {
        process::run_within(&mut self.command(args), DEADLINE, stdin)
    }

    /// An executable script of the sandbox.
    fn script(&self, name: &str, body: &str) -> String {
        let path = self.dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("a script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("its mode");
        path.display().to_string()
    }

    /// Whatever the editor's temporary directory still holds: nothing, after any run.
    fn left_behind(&self) -> Vec<String> {
        std::fs::read_dir(self.dir.join("scratch"))
            .map(|entries| {
                entries
                    .map(|e| e.expect("an entry").path().display().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.dir.join(name)).expect("the file")
    }

    fn mode(&self, name: &str) -> u32 {
        std::fs::metadata(self.dir.join(name))
            .expect("the file")
            .permissions()
            .mode()
            & 0o777
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[rstest]
#[case::encrypted_twice(&["encrypt", "vaulted", "--vault-password-file", "pw"], None,
    "[ERROR]: input is already encrypted\n", 1)]
#[case::missing_source(&["encrypt", "nosuch", "--vault-password-file", "pw"], None,
    "[ERROR]: Unable to read source file ({dir}/nosuch): [Errno 2] No such file or directory: '{dir}/nosuch'\n", 1)]
#[case::missing_source_to_decrypt(&["decrypt", "nosuch", "--vault-password-file", "pw"], None,
    "[ERROR]: Unable to read source file ({dir}/nosuch): [Errno 2] No such file or directory: '{dir}/nosuch'\n", 1)]
#[case::decrypt_a_plain_file(&["decrypt", "plain", "--vault-password-file", "pw"], None,
    "[ERROR]: Input is not vault encrypted data. for {dir}/plain\n", 1)]
#[case::view_a_plain_file_names_it_as_typed(&["view", "plain", "--vault-password-file", "pw"], None,
    "[ERROR]: Input is not vault encrypted data. for plain\n", 1)]
#[case::decrypt_with_a_wrong_password(&["decrypt", "vaulted", "--vault-password-file", "bad"], None,
    "[ERROR]: Decryption failed (no vault secrets were found that could decrypt). for {dir}/vaulted\n", 1)]
#[case::view_with_a_wrong_password(&["view", "vaulted", "--vault-password-file", "bad"], None,
    "[ERROR]: Decryption failed (no vault secrets were found that could decrypt). for vaulted\n", 1)]
#[case::output_with_two_files(&["encrypt", "plain", "vaulted", "--output", "out", "--vault-password-file", "pw"], None,
    "[ERROR]: At most one input file may be used with the --output option\n", 5)]
#[case::decrypt_output_with_two_files(&["decrypt", "plain", "vaulted", "--output", "out", "--vault-password-file", "pw"], None,
    "[ERROR]: At most one input file may be used with the --output option\n", 5)]
#[case::two_ids_and_no_choice(&["encrypt", "plain", "--vault-id", "lbl@pw", "--vault-id", "l2@bad"], None,
    "[ERROR]: The vault-ids lbl,l2 are available to encrypt. Specify the vault-id to encrypt with --encrypt-vault-id\n", 5)]
#[case::unknown_encrypt_id(&["encrypt", "plain", "--vault-id", "lbl@pw", "--vault-id", "l2@bad", "--encrypt-vault-id", "nope"], None,
    "[ERROR]: Did not find a match for --encrypt-vault-id=nope in the known vault-ids ['lbl', 'l2']\n", 1)]
#[case::encrypt_id_with_only_the_default(&["encrypt", "plain", "--vault-password-file", "pw", "--encrypt-vault-id", "lbl"], None,
    "[ERROR]: Did not find a match for --encrypt-vault-id=lbl in the known vault-ids ['default']\n", 1)]
#[case::semicolon_in_an_id(&["encrypt", "plain", "--vault-id", "a;b@pw"], None,
    "[ERROR]: 'a;b@pw' is not a valid vault id. The character ';' is not allowed in vault ids\n", 5)]
#[case::missing_password_file(&["view", "vaulted", "--vault-password-file", "nopw"], None,
    "[WARNING]: Error getting vault password file (default): The vault password file {dir}/nopw was not found\n[ERROR]: The vault password file {dir}/nopw was not found\n", 1)]
fn a_refusal_is_worded_as_the_reference_words_it(
    #[case] args: &[&str],
    #[case] stdin: Option<&[u8]>,
    #[case] want: &str,
    #[case] code: i32,
) {
    let sandbox = Sandbox::new();
    let out = sandbox.run(args, stdin);
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        want.replace("{dir}", &sandbox.dir.display().to_string())
    );
    assert_eq!(out.status.code(), Some(code));
}

/// The prompt goes to the terminal when there is one and to standard error otherwise, so only
/// the lines after it are held to the reference.
#[rstest]
#[case::no_password_at_all(&["view", "vaulted"], b"",
    "[WARNING]: Error in vault password prompt (default): EOFError (ctrl-d) on prompt for (default)\n[ERROR]: EOFError (ctrl-d) on prompt for (default)\n")]
#[case::new_password_not_given(&["encrypt", "plain", "-J"], b"",
    "[WARNING]: Error in vault password prompt (default): EOFError (ctrl-d) on prompt for (default)\n[ERROR]: EOFError (ctrl-d) on prompt for (default)\n")]
#[case::confirmation_differs(&["encrypt", "plain"], b"one\ntwo\n",
    "[WARNING]: Error in vault password prompt (default): Passwords do not match\n[ERROR]: Passwords do not match\n")]
fn a_prompt_that_fails_is_worded_as_the_reference_words_it(
    #[case] args: &[&str],
    #[case] typed: &[u8],
    #[case] want: &str,
) {
    let sandbox = Sandbox::new();
    let out = sandbox.run(args, Some(typed));
    assert!(text(&out.stderr).ends_with(want), "{}", text(&out.stderr));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(sandbox.read("plain"), b"plain\n");
}

#[test]
fn a_new_password_typed_twice_encrypts() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["encrypt", "plain"], Some(b"pw\npw\n"));
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = sandbox.run(&["view", "plain", "--vault-password-file", "pw"], None);
    assert_eq!(text(&out.stdout), "plain\n\n");
}

/// Mode 0600 whatever the source had, the header of the id, silence, and a source without a final
/// newline back byte for byte, read with another label.
#[rstest]
#[case::default_id(&["--vault-password-file", "pw"], "$ANSIBLE_VAULT;1.1;AES256")]
#[case::a_label(&["--vault-id", "lbl@pw"], "$ANSIBLE_VAULT;1.2;AES256;lbl")]
#[case::the_default_named(&["--vault-id", "default@pw"], "$ANSIBLE_VAULT;1.1;AES256")]
fn a_file_is_encrypted_in_place_and_decrypted_back(#[case] secret: &[&str], #[case] header: &str) {
    let sandbox = Sandbox::new();
    let file = sandbox.dir.join("data");
    std::fs::write(&file, "no final newline").expect("a file");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).expect("its mode");
    let args: Vec<&str> = ["encrypt", "data"]
        .into_iter()
        .chain(secret.iter().copied())
        .collect();
    let out = sandbox.run(&args, None);
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        (String::new(), String::new())
    );
    assert_eq!(out.status.code(), Some(0));
    let vaulted = text(&sandbox.read("data"));
    let mut lines = vaulted.lines();
    assert_eq!(lines.next(), Some(header));
    let body: Vec<&str> = lines.collect();
    assert!(
        body.iter()
            .all(|l| l.len() <= 80 && l.bytes().all(|b| b.is_ascii_hexdigit()))
    );
    assert!(body[..body.len() - 1].iter().all(|l| l.len() == 80));
    assert_eq!(sandbox.mode("data"), 0o600);

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("its mode");
    let out = sandbox.run(&["decrypt", "data", "--vault-id", "other@pw"], None);
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        (String::new(), String::new())
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(sandbox.read("data"), b"no final newline");
    assert_eq!(sandbox.mode("data"), 0o600);
}

#[test]
fn several_files_are_encrypted_and_viewed_in_order() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.dir.join("one"), "one").expect("a file");
    std::fs::write(sandbox.dir.join("two"), "two").expect("a file");
    let out = sandbox.run(
        &["encrypt", "one", "two", "--vault-password-file", "pw"],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    let out = sandbox.run(&["view", "one", "two", "--vault-password-file", "pw"], None);
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        ("one\ntwo\n".into(), String::new())
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(
        sandbox
            .read("one")
            .starts_with(b"$ANSIBLE_VAULT;1.1;AES256\n")
    );
}

#[test]
fn output_leaves_the_source_alone() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(
        &[
            "encrypt",
            "plain",
            "--output",
            "out",
            "--vault-password-file",
            "pw",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(sandbox.read("plain"), b"plain\n");
    assert!(
        sandbox
            .read("out")
            .starts_with(b"$ANSIBLE_VAULT;1.1;AES256\n")
    );
    assert_eq!(sandbox.mode("out"), 0o600);

    let out = sandbox.run(
        &[
            "decrypt",
            "out",
            "--output",
            "back",
            "--vault-password-file",
            "pw",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(sandbox.read("back"), b"plain\n");
    assert!(sandbox.read("out").starts_with(b"$ANSIBLE_VAULT"));
}

#[test]
fn a_dash_is_standard_input_and_standard_output() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(
        &["encrypt", "--vault-password-file", "pw"],
        Some(b"from stdin"),
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stderr), "");
    assert!(out.stdout.starts_with(b"$ANSIBLE_VAULT;1.1;AES256\n"));
    let out = sandbox.run(
        &[
            "decrypt",
            "-",
            "--output",
            "-",
            "--vault-password-file",
            "pw",
        ],
        Some(&out.stdout),
    );
    assert_eq!(out.stdout, b"from stdin");
    let out = sandbox.run(
        &[
            "encrypt",
            "plain",
            "--output",
            "-",
            "--vault-password-file",
            "pw",
        ],
        None,
    );
    assert!(out.stdout.starts_with(b"$ANSIBLE_VAULT;1.1;AES256\n"));
    assert_eq!(sandbox.read("plain"), b"plain\n");
}

#[test]
fn the_secret_comes_from_the_environment_and_the_configuration() {
    let sandbox = Sandbox::new();
    let mut command = sandbox.command(&["view", "vaulted"]);
    command.env("ANSIBLE_VAULT_PASSWORD_FILE", sandbox.dir.join("pw"));
    let out = process::run_within(&mut command, DEADLINE, None);
    assert_eq!(text(&out.stdout), "secret\n\n");

    std::fs::write(
        sandbox.dir.join("ansible.cfg"),
        "[defaults]\nvault_password_file = pw\n",
    )
    .expect("a config");
    let out = sandbox.run(&["view", "vaulted"], None);
    assert_eq!(text(&out.stdout), "secret\n\n");
    let out = sandbox.run(&["view", "vaulted", "-v"], None);
    assert!(text(&out.stdout).starts_with(&format!(
        "Using {}/ansible.cfg as config file\n",
        sandbox.dir.display()
    )));
}

#[test]
fn the_configured_encrypt_identity_picks_the_secret() {
    let sandbox = Sandbox::new();
    let mut command = sandbox.command(&[
        "encrypt",
        "plain",
        "--vault-id",
        "bad@bad",
        "--vault-id",
        "zz@pw",
    ]);
    command.env("ANSIBLE_VAULT_ENCRYPT_IDENTITY", "zz");
    let out = process::run_within(&mut command, DEADLINE, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        sandbox
            .read("plain")
            .starts_with(b"$ANSIBLE_VAULT;1.2;AES256;zz\n")
    );
}

#[test]
fn version_names_ansible_vault() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["--version"], None);
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out.stdout).starts_with("ansible-vault [core 2.19.12]\n"));
}

/// A pseudo-terminal: the child's end, and ours to type into and read from.
fn terminal() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: both descriptors are written by `openpty` and owned by nothing else.
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "a pseudo-terminal opens");
    // SAFETY: `openpty` just returned them, and each is wrapped once.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn type_ahead(master: &OwnedFd, bytes: &[u8]) {
    // SAFETY: `bytes` is valid for its length and `master` is open.
    let written = unsafe { libc::write(master.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
    assert_eq!(usize::try_from(written).ok(), Some(bytes.len()));
}

/// What the terminal shows once the child is gone.
fn screen(master: &OwnedFd) -> String {
    // SAFETY: plain flag changes on a descriptor we own; what is left to read is already queued.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut shown = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        // SAFETY: `chunk` is valid for its length and `master` is open.
        let n = unsafe { libc::read(master.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
        match usize::try_from(n) {
            Ok(n) if n > 0 => shown.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    text(&shown)
}

/// Waits for the child, killing it at the deadline, and returns its code and standard error.
fn finish(mut child: std::process::Child) -> (i32, String, Vec<u8>) {
    use std::io::Read as _;
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child is waited for") {
            break status;
        }
        assert!(start.elapsed() < DEADLINE, "the command did not finish");
        std::thread::sleep(Duration::from_millis(10));
    };
    let (mut stderr, mut stdout) = (Vec::new(), Vec::new());
    child
        .stderr
        .take()
        .expect("piped")
        .read_to_end(&mut stderr)
        .expect("its stderr");
    if let Some(mut piped) = child.stdout.take() {
        piped.read_to_end(&mut stdout).expect("its stdout");
    }
    (status.code().unwrap_or(-1), text(&stderr), stdout)
}

/// Standard output on a terminal: the confirmations on standard error and the pager.
#[rstest]
#[case::encrypt(&["encrypt", "plain", "--vault-password-file", "pw"], "Encryption successful\n", "")]
#[case::decrypt(&["decrypt", "vaulted", "--vault-password-file", "pw"], "Decryption successful\n", "")]
#[case::view_through_the_pager(&["view", "vaulted", "--vault-password-file", "pw"], "", "SECRET\r\n")]
fn standard_output_on_a_terminal_changes_what_is_said(
    #[case] args: &[&str],
    #[case] said: &str,
    #[case] shown: &str,
) {
    let sandbox = Sandbox::new();
    let (master, slave) = terminal();
    // Our own copy of the slave end, open until the screen is read: where the last slave
    // descriptor closing discards what the master has not read (macOS), the child exiting first
    // would otherwise leave nothing to read.
    let keep = slave.try_clone().expect("the slave end is duplicated");
    let mut command = sandbox.command(args);
    // A pager that is not `less`, and that proves the text went through it.
    command
        .env("PAGER", "tr a-z A-Z")
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped());
    let child = command.spawn().expect("volant starts");
    drop(command);
    let (code, stderr, _) = finish(child);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr, said);
    assert_eq!(screen(&master), shown);
    drop(keep);
}

#[test]
fn standard_input_on_a_terminal_is_announced() {
    let sandbox = Sandbox::new();
    let (master, slave) = terminal();
    type_ahead(&master, b"typed\n\x04");
    let mut command = sandbox.command(&["encrypt", "--vault-password-file", "pw"]);
    command
        .stdin(Stdio::from(slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("volant starts");
    drop(command);
    let (code, stderr, stdout) = finish(child);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr, "Reading plaintext input from stdin\n");
    assert!(stdout.starts_with(b"$ANSIBLE_VAULT;1.1;AES256\n"));
}

/// ansible-vault itself, run by the recorded ansible-core's interpreter.
fn reference(python: &Path, sandbox: &Sandbox, args: &[&str]) {
    let out = Command::new(python)
        .args([
            "-c",
            "from ansible.cli.vault import VaultCLI; VaultCLI.cli_executor()",
        ])
        .args(args)
        .current_dir(&sandbox.dir)
        .stdin(Stdio::null())
        .output()
        .expect("the reference starts");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

/// A file one tool wrote, the other reads, in both directions, for the default id and a label.
#[rstest]
#[case::default_id(&["--vault-password-file", "pw"])]
#[case::a_label(&["--vault-id", "lbl@pw"])]
fn a_file_one_tool_encrypts_the_other_decrypts(#[case] secret: &[&str]) {
    let Some(python) = python::reference_python() else {
        return;
    };
    let sandbox = Sandbox::new();
    let original: &[u8] = b"no final newline";
    for volant_encrypts in [false, true] {
        std::fs::write(sandbox.dir.join("data"), original).expect("a file");
        let encrypt: Vec<&str> = ["encrypt", "data"]
            .into_iter()
            .chain(secret.iter().copied())
            .collect();
        let decrypt: Vec<&str> = ["decrypt", "data"]
            .into_iter()
            .chain(secret.iter().copied())
            .collect();
        if volant_encrypts {
            assert_eq!(sandbox.run(&encrypt, None).status.code(), Some(0));
            reference(&python, &sandbox, &decrypt);
        } else {
            reference(&python, &sandbox, &encrypt);
            assert_eq!(sandbox.run(&decrypt, None).status.code(), Some(0));
        }
        assert_eq!(
            sandbox.read("data"),
            original,
            "volant_encrypts={volant_encrypts}"
        );
    }
}

/// A write that cannot finish leaves the original as it was, and no temporary file behind.
#[test]
fn a_failed_write_keeps_the_original() {
    let sandbox = Sandbox::new();
    std::fs::create_dir(sandbox.dir.join("target_dir")).expect("a directory");
    let before = sandbox.read("plain");
    // The rename over a directory fails after the new file was written.
    let out = sandbox.run(
        &[
            "encrypt",
            "plain",
            "--output",
            "target_dir",
            "--vault-password-file",
            "pw",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).starts_with("[ERROR]: Unable to replace "),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(sandbox.read("plain"), before);
    let mut names: Vec<String> = std::fs::read_dir(&sandbox.dir)
        .expect("the directory")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(names, ["bad", "plain", "pw", "target_dir", "vaulted"]);

    // A directory that cannot be written to: the file is untouched (root writes anyway).
    std::fs::set_permissions(&sandbox.dir, std::fs::Permissions::from_mode(0o500))
        .expect("its mode");
    let out = sandbox.run(&["encrypt", "plain", "--vault-password-file", "pw"], None);
    std::fs::set_permissions(&sandbox.dir, std::fs::Permissions::from_mode(0o700))
        .expect("its mode");
    if out.status.code() != Some(0) {
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(sandbox.read("plain"), before);
    }
}

/// No subcommand is argparse's error on standard error, naming the program as started.
#[rstest]
#[case::as_ansible_vault(
    "ansible-vault",
    "usage: ansible-vault [-h] [--version] [-v] {encrypt,decrypt,view} ...\nansible-vault: error: the following arguments are required: action\n"
)]
#[case::as_volant_vault(
    "volant",
    "usage: volant vault [-h] [--version] [-v] {encrypt,decrypt,view} ...\nvolant vault: error: the following arguments are required: action\n"
)]
fn no_subcommand_is_a_usage_error(#[case] name: &str, #[case] want: &str) {
    let sandbox = Sandbox::new();
    let mut command = if name == "volant" {
        let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
        command.arg("vault");
        command
    } else {
        let link = sandbox.dir.join(name);
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_volant"), &link).expect("a symlink");
        Command::new(link)
    };
    command.current_dir(&sandbox.dir).stdin(Stdio::null());
    let out = process::run_within(&mut command, DEADLINE, None);
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        (String::new(), want.to_string())
    );
    assert_eq!(out.status.code(), Some(2));
}

const PW: &[&str] = &["--vault-password-file", "pw"];

impl Sandbox {
    /// Runs `args` with `editor` as the editor.
    fn edit_with(&self, editor: &str, args: &[&str]) -> Output {
        let mut command = self.command(args);
        command.env("EDITOR", editor);
        process::run_within(&mut command, DEADLINE, None)
    }
}

#[test]
fn create_writes_what_the_editor_leaves_encrypted() {
    let sandbox = Sandbox::new();
    let editor = sandbox.script("ed.sh", "echo hello > \"$1\"");
    let args = ["create", "sub/new.yml", "--skip-tty-check", PW[0], PW[1]];
    let out = sandbox.edit_with(&editor, &args);
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        (
            String::new(),
            "[WARNING]: sub does not exist, creating...\n".into()
        )
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(
        sandbox
            .read("sub/new.yml")
            .starts_with(b"$ANSIBLE_VAULT;1.1;AES256\n")
    );
    assert_eq!(sandbox.mode("sub/new.yml"), 0o600);
    let out = sandbox.run(&["view", "sub/new.yml", PW[0], PW[1]], None);
    assert_eq!(text(&out.stdout), "hello\n\n");
    assert_eq!(sandbox.left_behind(), Vec::<String>::new());
}

#[test]
fn create_with_an_editor_that_writes_nothing_gives_an_empty_vault_text() {
    let sandbox = Sandbox::new();
    let editor = sandbox.script("nop.sh", "exit 0");
    let out = sandbox.edit_with(
        &editor,
        &["create", "empty", "--skip-tty-check", PW[0], PW[1]],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = sandbox.run(&["view", "empty", PW[0], PW[1]], None);
    assert_eq!(text(&out.stdout), "\n");
}

#[test]
fn the_editor_line_is_split_like_a_shell_line_and_the_last_environment_name_wins() {
    let sandbox = Sandbox::new();
    let editor = sandbox.script("args.sh", "printf '%s' \"$1\" > \"$2\"");
    let mut command = sandbox.command(&["create", "flag", "--skip-tty-check", PW[0], PW[1]]);
    command
        .env("ANSIBLE_EDITOR", "false")
        .env("EDITOR", format!("{editor} --flag"));
    let out = process::run_within(&mut command, DEADLINE, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = sandbox.run(&["view", "flag", PW[0], PW[1]], None);
    assert_eq!(text(&out.stdout), "--flag\n");
}

#[rstest]
#[case::exists(&["create", "plain", "--skip-tty-check"], "[ERROR]: plain exists, please use 'edit' instead\n", 1)]
#[case::two_files(&["create", "a", "b", "--skip-tty-check"], "[ERROR]: ansible-vault create can take only one filename argument\n", 5)]
#[case::no_terminal(&["create", "fresh"], "[ERROR]: not a tty, editor cannot be opened\n", 5)]
#[case::edit_plain(&["edit", "plain"], "[ERROR]: Input is not vault encrypted data. for {dir}/plain\n", 1)]
#[case::edit_missing(&["edit", "nosuch"], "[ERROR]: Unable to read source file ({dir}/nosuch): [Errno 2] No such file or directory: '{dir}/nosuch'\n", 1)]
#[case::edit_wrong_password(&["edit", "vaulted", "--vault-password-file", "bad"], "[ERROR]: Decryption failed (no vault secrets were found that could decrypt). for {dir}/vaulted\n", 1)]
#[case::rekey_plain(&["rekey", "plain", "--new-vault-password-file", "bad"], "[ERROR]: Input is not vault encrypted data. for {dir}/plain\n", 1)]
#[case::rekey_missing(&["rekey", "nosuch", "--new-vault-password-file", "bad"], "[ERROR]: Unexpected Exception, this is probably a bug: [Errno 2] No such file or directory: '{dir}/nosuch'\n", 250)]
#[case::rekey_unknown_encrypt_id(&["rekey", "vaulted", "--new-vault-id", "a@bad", "--encrypt-vault-id", "zz"], "[ERROR]: Did not find a match for --encrypt-vault-id=zz in the known vault-ids ['a']\n", 1)]
fn a_refusal_of_the_editing_commands_is_worded_as_the_reference_words_it(
    #[case] args: &[&str],
    #[case] want: &str,
    #[case] code: i32,
) {
    let sandbox = Sandbox::new();
    let editor = sandbox.script("must-not-run.sh", "touch ran");
    let mut all: Vec<&str> = args.to_vec();
    if !all.contains(&"--vault-password-file") {
        all.extend(PW);
    }
    let out = sandbox.edit_with(&editor, &all);
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        want.replace("{dir}", &sandbox.dir.display().to_string())
    );
    assert_eq!(out.status.code(), Some(code));
    assert!(!sandbox.dir.join("ran").exists(), "the editor ran");
    assert!(!sandbox.dir.join("fresh").exists());
    assert_eq!(sandbox.read("plain"), b"plain\n");
    assert_eq!(sandbox.left_behind(), Vec::<String>::new());
}

#[test]
fn a_missing_editor_is_named_and_leaves_nothing() {
    let sandbox = Sandbox::new();
    let out = sandbox.edit_with(
        "no-such-editor-here",
        &["create", "fresh", "--skip-tty-check", PW[0], PW[1]],
    );
    let stderr = text(&out.stderr);
    assert!(
        stderr.starts_with("[ERROR]: Unable to execute the command \"no-such-editor-here ")
            && stderr.ends_with("\": [Errno 2] No such file or directory: 'no-such-editor-here'\n"),
        "{stderr}"
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(!sandbox.dir.join("fresh").exists());
    assert_eq!(sandbox.left_behind(), Vec::<String>::new());
}

/// An editor that changes the text, one that does not, and one that fails: the file keeps its
/// mode, and its label, and is not rewritten when nothing changed.
#[test]
fn edit_rewrites_only_what_changed_and_keeps_the_mode_and_the_label() {
    let sandbox = Sandbox::new();
    let change = sandbox.script("change.sh", "echo changed > \"$1\"");
    let same = sandbox.script("same.sh", "exit 0");
    let fail = sandbox.script("fail.sh", "exit 3");
    std::fs::write(sandbox.dir.join("l.yml"), "one").expect("a file");
    let labelled = ["--vault-id", "lbl@pw"];
    let out = sandbox.run(&["encrypt", "l.yml", labelled[0], labelled[1]], None);
    assert_eq!(out.status.code(), Some(0));
    std::fs::set_permissions(
        sandbox.dir.join("l.yml"),
        std::fs::Permissions::from_mode(0o640),
    )
    .expect("its mode");
    let before = sandbox.read("l.yml");

    for editor in [&same, &fail] {
        let out = sandbox.edit_with(editor, &["edit", "l.yml", labelled[0], labelled[1]]);
        assert_eq!(
            (text(&out.stdout), text(&out.stderr)),
            (String::new(), String::new())
        );
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(
            sandbox.read("l.yml"),
            before,
            "unchanged text, unchanged file"
        );
        assert_eq!(sandbox.left_behind(), Vec::<String>::new());
    }

    let out = sandbox.edit_with(&change, &["edit", "l.yml", labelled[0], labelled[1]]);
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        (String::new(), String::new())
    );
    assert_ne!(sandbox.read("l.yml"), before);
    assert!(
        sandbox
            .read("l.yml")
            .starts_with(b"$ANSIBLE_VAULT;1.2;AES256;lbl\n")
    );
    assert_eq!(sandbox.mode("l.yml"), 0o640);
    let out = sandbox.run(&["view", "l.yml", "--vault-id", "other@pw"], None);
    assert_eq!(text(&out.stdout), "changed\n\n");
    assert_eq!(sandbox.left_behind(), Vec::<String>::new());
}

/// To a password and to a label: the header, the mode, the message on standard error with
/// standard output redirected, and the text under the new secret.
#[rstest]
#[case::to_a_password(&["--new-vault-password-file", "pw2"], &["--vault-password-file", "pw2"], "$ANSIBLE_VAULT;1.1;AES256")]
#[case::to_a_label(&["--new-vault-id", "lbl@pw2"], &["--vault-id", "lbl@pw2"], "$ANSIBLE_VAULT;1.2;AES256;lbl")]
fn rekey_changes_the_secret_and_nothing_else(
    #[case] new: &[&str],
    #[case] then: &[&str],
    #[case] header: &str,
) {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.dir.join("pw2"), "pw2\n").expect("a password");
    std::fs::set_permissions(
        sandbox.dir.join("vaulted"),
        std::fs::Permissions::from_mode(0o640),
    )
    .expect("its mode");
    let mut args = vec!["rekey", "vaulted", PW[0], PW[1]];
    args.extend(new);
    let out = sandbox.run(&args, None);
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        (String::new(), "Rekey successful\n".into())
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(
        sandbox
            .read("vaulted")
            .starts_with(format!("{header}\n").as_bytes())
    );
    assert_eq!(sandbox.mode("vaulted"), 0o640);
    let mut view = vec!["view", "vaulted"];
    view.extend(then);
    assert_eq!(text(&sandbox.run(&view, None).stdout), "secret\n\n");
    let old = sandbox.run(&["view", "vaulted", PW[0], PW[1]], None);
    assert_eq!(old.status.code(), Some(1));
}

#[test]
fn rekey_with_two_new_ids_takes_the_last_given() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.dir.join("pw2"), "pw2\n").expect("a password");
    let out = sandbox.run(
        &[
            "rekey",
            "vaulted",
            PW[0],
            PW[1],
            "--new-vault-id",
            "a@pw",
            "--new-vault-id",
            "b@pw2",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        sandbox
            .read("vaulted")
            .starts_with(b"$ANSIBLE_VAULT;1.2;AES256;b\n")
    );
}

#[test]
fn the_two_new_secret_options_exclude_each_other() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(
        &[
            "rekey",
            "vaulted",
            PW[0],
            PW[1],
            "--new-vault-id",
            "a@pw",
            "--new-vault-password-file",
            "pw",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn rekey_without_a_new_secret_asks_for_one_and_changes_nothing_when_refused() {
    let sandbox = Sandbox::new();
    let before = sandbox.read("vaulted");
    let out = sandbox.run(&["rekey", "vaulted", PW[0], PW[1]], Some(b""));
    assert!(text(&out.stderr).ends_with(
        "[WARNING]: Error in vault password prompt (default): EOFError (ctrl-d) on prompt for (default)\n[ERROR]: EOFError (ctrl-d) on prompt for (default)\n"
    ));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(sandbox.read("vaulted"), before);
}

/// A terminated edit stops the editor and leaves no plaintext: the copy is private while it
/// exists and gone afterwards.
#[test]
fn a_terminated_edit_leaves_no_temporary_file() {
    let sandbox = Sandbox::new();
    let editor = sandbox.script("hang.sh", "echo \"$1\" > started; exec sleep 60");
    let mut command = sandbox.command(&["edit", "vaulted", PW[0], PW[1]]);
    command.env("EDITOR", &editor).stderr(Stdio::piped());
    let child = command.spawn().expect("volant starts");
    let started = sandbox.dir.join("started");
    let begin = std::time::Instant::now();
    while std::fs::read_to_string(&started).map_or(true, |s| !s.ends_with('\n')) {
        assert!(begin.elapsed() < DEADLINE, "the editor never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    let scratch = PathBuf::from(std::fs::read_to_string(&started).expect("its path").trim());
    let mode = |path: &Path| {
        std::fs::metadata(path)
            .expect("it exists")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&scratch), 0o600);
    assert_eq!(mode(scratch.parent().expect("its directory")), 0o700);
    assert_eq!(
        std::fs::read_to_string(&scratch).expect("the copy"),
        "secret\n"
    );
    let pid = libc::pid_t::try_from(child.id()).expect("a pid");
    // SAFETY: the pid is our own child's.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let (code, _, _) = finish(child);
    assert_eq!(code, 128 + libc::SIGTERM);
    assert!(!scratch.exists());
    assert_eq!(sandbox.left_behind(), Vec::<String>::new());
    assert!(sandbox.read("vaulted").starts_with(b"$ANSIBLE_VAULT"));
}

/// Standard output on a terminal is what `create` asks for.
#[test]
fn create_runs_the_editor_when_standard_output_is_a_terminal() {
    let sandbox = Sandbox::new();
    let editor = sandbox.script("ed.sh", "echo hello > \"$1\"");
    let (master, slave) = terminal();
    let keep = slave.try_clone().expect("the slave end is duplicated");
    let mut command = sandbox.command(&["create", "fresh", PW[0], PW[1]]);
    command
        .env("EDITOR", &editor)
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped());
    let child = command.spawn().expect("volant starts");
    drop(command);
    let (code, stderr, _) = finish(child);
    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(screen(&master), "");
    drop(keep);
    assert!(
        sandbox
            .read("fresh")
            .starts_with(b"$ANSIBLE_VAULT;1.1;AES256\n")
    );
}

/// A file one tool rekeys is read by the other under the new secret.
#[test]
fn a_file_one_tool_rekeys_the_other_reads_under_the_new_secret() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.dir.join("pw2"), "pw2\n").expect("a password");
    reference(
        &python,
        &sandbox,
        &[
            "rekey",
            "vaulted",
            PW[0],
            PW[1],
            "--new-vault-password-file",
            "pw2",
        ],
    );
    let out = sandbox.run(&["view", "vaulted", "--vault-password-file", "pw2"], None);
    assert_eq!(text(&out.stdout), "secret\n\n");
    let out = sandbox.run(
        &[
            "rekey",
            "vaulted",
            "--vault-password-file",
            "pw2",
            "--new-vault-id",
            "lbl@pw",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    reference(
        &python,
        &sandbox,
        &["decrypt", "vaulted", "--vault-id", "lbl@pw"],
    );
    assert_eq!(sandbox.read("vaulted"), b"secret\n");
}

/// A request to stop sent to the whole process group, as a closed terminal does: the editor dies
/// with the command, and the file is left as it was with no copy behind.
#[rstest]
#[case::hangup(libc::SIGHUP)]
#[case::terminate(libc::SIGTERM)]
fn a_signal_to_the_process_group_stops_an_edit_without_writing(#[case] signal: i32) {
    use std::os::unix::process::CommandExt as _;
    let sandbox = Sandbox::new();
    let editor = sandbox.script(
        "hang.sh",
        "echo \"$1\" > started; echo changed > \"$1\"; exec sleep 60",
    );
    let before = sandbox.read("vaulted");
    let mut command = sandbox.command(&["edit", "vaulted", PW[0], PW[1]]);
    command
        .env("EDITOR", &editor)
        .stderr(Stdio::piped())
        .process_group(0);
    let child = command.spawn().expect("volant starts");
    let started = sandbox.dir.join("started");
    let begin = std::time::Instant::now();
    while std::fs::read_to_string(&started).map_or(true, |s| !s.ends_with('\n')) {
        assert!(begin.elapsed() < DEADLINE, "the editor never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The editor has changed its copy by now: that is what must not be written.
    std::thread::sleep(Duration::from_millis(200));
    let pgid = libc::pid_t::try_from(child.id()).expect("a pid");
    // SAFETY: the group is our child's own, made by `process_group(0)`.
    assert_eq!(unsafe { libc::kill(-pgid, signal) }, 0);
    let (code, _, _) = finish(child);
    assert_eq!(code, 128 + signal);
    assert_eq!(sandbox.read("vaulted"), before);
    assert_eq!(sandbox.left_behind(), Vec::<String>::new());
}

#[test]
fn the_editor_comes_from_the_configuration_without_its_inline_comment() {
    let sandbox = Sandbox::new();
    let editor = sandbox.script("ed.sh", "echo hello > \"$1\"");
    std::fs::write(
        sandbox.dir.join("ansible.cfg"),
        format!("[defaults]\neditor = {editor} ; the usual one\n"),
    )
    .expect("a config");
    let out = sandbox.run(&["create", "fresh", "--skip-tty-check", PW[0], PW[1]], None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(sandbox.read("fresh").starts_with(b"$ANSIBLE_VAULT"));
}

#[test]
fn create_refuses_a_directory_and_works_without_home() {
    let sandbox = Sandbox::new();
    std::fs::create_dir(sandbox.dir.join("adir")).expect("a directory");
    let editor = sandbox.script("ed.sh", "echo hello > \"$1\"");
    let out = sandbox.edit_with(
        &editor,
        &["create", "adir", "--skip-tty-check", PW[0], PW[1]],
    );
    assert_eq!(text(&out.stderr), "[ERROR]: adir is a directory\n");
    assert_eq!(out.status.code(), Some(1));
    let mut command = sandbox.command(&["create", "fresh", "--skip-tty-check", PW[0], PW[1]]);
    command.env("EDITOR", &editor).env_remove("HOME");
    let out = process::run_within(&mut command, DEADLINE, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

/// The `!vault |` blocks of `encrypt_string` output: the first line of each, and its ciphertext
/// with the ten spaces of indentation taken off.
fn blocks(output: &str) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("          ") {
            let (_, text) = found.last_mut().expect("a block starts first");
            text.push_str(rest);
            text.push('\n');
        } else {
            assert!(
                line.ends_with("!vault |"),
                "a block opens with !vault |: {line}"
            );
            found.push((line.to_string(), String::new()));
        }
    }
    found
}

impl Sandbox {
    /// What a block opens to, through `volant vault decrypt`.
    fn reveal(&self, ciphertext: &str) -> Vec<u8> {
        std::fs::write(self.dir.join("block"), ciphertext).expect("a block");
        let out = self.run(&["decrypt", "block", "--output", "-", PW[0], PW[1]], None);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        out.stdout
    }
}

#[rstest]
#[case::no_name(&["foobar"], "!vault |", "$ANSIBLE_VAULT;1.1;AES256")]
#[case::a_name(&["-n", "mykey", "foobar"], "mykey: !vault |", "$ANSIBLE_VAULT;1.1;AES256")]
#[case::long_name(&["--name", "mykey", "foobar"], "mykey: !vault |", "$ANSIBLE_VAULT;1.1;AES256")]
fn a_string_becomes_one_block_with_the_references_shape(
    #[case] args: &[&str],
    #[case] first: &str,
    #[case] header: &str,
) {
    let sandbox = Sandbox::new();
    let mut all = vec!["encrypt_string", PW[0], PW[1]];
    all.extend(args);
    let out = sandbox.run(&all, None);
    assert_eq!(text(&out.stderr), "");
    assert_eq!(out.status.code(), Some(0));
    let stdout = text(&out.stdout);
    assert!(stdout.ends_with('\n') && !stdout.ends_with("\n\n"));
    let found = blocks(&stdout);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, first);
    assert!(found[0].1.starts_with(&format!("{header}\n")));
    assert!(stdout.lines().skip(1).all(|l| l.starts_with("          ")));
    assert_eq!(sandbox.reveal(&found[0].1), b"foobar");
}

#[test]
fn a_label_gives_a_version_1_2_header() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["encrypt_string", "--vault-id", "lbl@pw", "x"], None);
    let found = blocks(&text(&out.stdout));
    assert!(found[0].1.starts_with("$ANSIBLE_VAULT;1.2;AES256;lbl\n"));
    assert_eq!(sandbox.reveal(&found[0].1), b"x");
}

#[rstest]
#[case::fewer_names(&["one", "two", "-n", "k"],
    "The number of --name options do not match the number of args.\nThe last named variable will be \"k\". The rest will not have names.\n# The encrypted version of variable (\"k\", the string #1 from the command line args).\n# The encrypted version of the string #2 from the command line args.)\n",
    &["k: !vault |", "!vault |"])]
#[case::more_names(&["one", "two", "-n", "k", "-n", "j", "-n", "m"],
    "# The encrypted version of variable (\"k\", the string #1 from the command line args).\n# The encrypted version of variable (\"j\", the string #2 from the command line args).\n",
    &["k: !vault |", "j: !vault |"])]
#[case::no_names(&["one", "two"],
    "# The encrypted version of the string #1 from the command line args.)\n# The encrypted version of the string #2 from the command line args.)\n",
    &["!vault |", "!vault |"])]
fn several_strings_print_blocks_in_order_and_comment_on_standard_error(
    #[case] args: &[&str],
    #[case] want_stderr: &str,
    #[case] firsts: &[&str],
) {
    let sandbox = Sandbox::new();
    let mut all = vec!["encrypt_string", PW[0], PW[1]];
    all.extend(args);
    let out = sandbox.run(&all, None);
    assert_eq!(text(&out.stderr), want_stderr);
    assert_eq!(out.status.code(), Some(0));
    let found = blocks(&text(&out.stdout));
    assert_eq!(
        found.iter().map(|b| b.0.as_str()).collect::<Vec<_>>(),
        firsts
    );
    assert_eq!(sandbox.reveal(&found[0].1), b"one");
    assert_eq!(sandbox.reveal(&found[1].1), b"two");
}

#[test]
fn text_is_kept_byte_for_byte_whatever_way_it_comes() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["encrypt_string", PW[0], PW[1], "tail\n"], None);
    assert_eq!(sandbox.reveal(&blocks(&text(&out.stdout))[0].1), b"tail\n");
    let out = sandbox.run(
        &["encrypt_string", PW[0], PW[1]],
        Some("a\r\nb\rc \u{e9}\u{e8}".as_bytes()),
    );
    assert_eq!(text(&out.stderr), "");
    assert_eq!(
        sandbox.reveal(&blocks(&text(&out.stdout))[0].1),
        "a\r\nb\rc \u{e9}\u{e8}".as_bytes()
    );
    let out = sandbox.run(
        &[
            "encrypt_string",
            PW[0],
            PW[1],
            "--stdin-name",
            "sn",
            "foo",
            "-",
        ],
        Some(b"in"),
    );
    assert_eq!(
        text(&out.stderr),
        "# The encrypted version of variable (\"sn\", the string #1 from stdin).\n# The encrypted version of the string #2 from the command line args.)\n"
    );
    let found = blocks(&text(&out.stdout));
    assert_eq!(found[0].0, "sn: !vault |");
    assert_eq!(sandbox.reveal(&found[0].1), b"in");
    assert_eq!(sandbox.reveal(&found[1].1), b"foo");
}

#[rstest]
#[case::empty_argument(&["encrypt_string", ""], None, "[ERROR]: The plaintext provided from the command line args was empty, not encrypting\n", 5)]
#[case::empty_stdin(&["encrypt_string"], Some(&b""[..]), "[ERROR]: stdin was empty, not encrypting\n", 5)]
#[case::empty_stdin_named(&["encrypt_string", "--stdin-name", "sn"], Some(&b""[..]), "[ERROR]: stdin was empty, not encrypting\n", 5)]
#[case::prompt_and_stdin(&["encrypt_string", "-p", "-"], None, "[ERROR]: The --prompt option is not supported if also reading input from stdin\n", 5)]
#[case::prompt_and_stdin_name(&["encrypt_string", "--prompt", "--stdin-name", "x"], None, "[ERROR]: The --prompt option is not supported if also reading input from stdin\n", 5)]
#[case::output_with_two(&["encrypt_string", "a", "b", "--output", "out"], None, "[ERROR]: At most one input file may be used with the --output option\n", 5)]
#[case::not_utf8(&["encrypt_string"], Some(&b"\xff\xfe"[..]), "[ERROR]: Unexpected Exception, this is probably a bug: 'utf-8' codec can't decode byte 0xff in position 0: invalid start byte\n", 250)]
fn a_refusal_of_encrypt_string_is_worded_as_the_reference_words_it(
    #[case] args: &[&str],
    #[case] stdin: Option<&[u8]>,
    #[case] want: &str,
    #[case] code: i32,
) {
    let sandbox = Sandbox::new();
    let mut all = args.to_vec();
    all.extend(PW);
    let out = sandbox.run(&all, stdin);
    assert_eq!(text(&out.stdout), "");
    assert_eq!(text(&out.stderr), want);
    assert_eq!(out.status.code(), Some(code));
}

#[test]
fn output_writes_the_blocks_to_a_file() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(
        &[
            "encrypt_string",
            PW[0],
            PW[1],
            "-n",
            "k",
            "--output",
            "vars.yml",
            "foobar",
        ],
        None,
    );
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        (String::new(), String::new())
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(sandbox.mode("vars.yml"), 0o600);
    let found = blocks(&text(&sandbox.read("vars.yml")));
    assert_eq!(found[0].0, "k: !vault |");
    assert_eq!(sandbox.reveal(&found[0].1), b"foobar");
}

/// The answers typed ahead on standard input: the name is echoed, the string is not.
#[rstest]
#[case::hidden(&["-p"], b"k\nsecret\n", "Variable name (enter for no name): k: !vault |\n")]
#[case::shown(&["-p", "--show-input"], b"k\nsecret\n", "Variable name (enter for no name): String to encrypt:k: !vault |\n")]
#[case::no_name(&["-p"], b"\nsecret\n", "Variable name (enter for no name): !vault |\n")]
fn the_prompt_asks_for_a_name_then_a_string(
    #[case] args: &[&str],
    #[case] typed: &[u8],
    #[case] starts: &str,
) {
    let sandbox = Sandbox::new();
    let mut all = vec!["encrypt_string", PW[0], PW[1]];
    all.extend(args);
    let out = sandbox.run(&all, Some(typed));
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.starts_with(starts), "{stdout}");
    let body = stdout
        .strip_prefix("Variable name (enter for no name): ")
        .expect("the name prompt comes first")
        .replacen("String to encrypt:", "", 1);
    let found = blocks(&body);
    assert_eq!(sandbox.reveal(&found[0].1), b"secret");
}

#[rstest]
#[case::no_string(
    b"k\n\n",
    "[ERROR]: The plaintext provided from the prompt was empty, not encrypting\n",
    5
)]
#[case::no_input(
    b"",
    "[ERROR]: Unexpected Exception, this is probably a bug: EOF when reading a line\n",
    250
)]
fn a_prompt_with_nothing_to_encrypt_stops(
    #[case] typed: &[u8],
    #[case] tail: &str,
    #[case] code: i32,
) {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["encrypt_string", "-p", PW[0], PW[1]], Some(typed));
    assert!(text(&out.stderr).ends_with(tail), "{}", text(&out.stderr));
    assert_eq!(out.status.code(), Some(code));
}

/// On a terminal: the notice before reading standard input, the message before the block.
#[test]
fn standard_input_and_output_on_a_terminal_add_their_notices() {
    let sandbox = Sandbox::new();
    let (master, slave) = terminal();
    let keep = slave.try_clone().expect("the slave end is duplicated");
    type_ahead(&master, b"abc\n\x04");
    let mut command = sandbox.command(&["encrypt_string", PW[0], PW[1]]);
    command
        .stdin(Stdio::from(slave.try_clone().expect("duplicated")))
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped());
    let child = command.spawn().expect("volant starts");
    drop(command);
    let (code, stderr, _) = finish(child);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stderr,
        "Reading plaintext input from stdin. (ctrl-d to end input, twice if your content does not already have a newline)\nEncryption successful\n"
    );
    let shown = screen(&master);
    assert!(
        shown.contains("!vault |\r\n          $ANSIBLE_VAULT;1.1;AES256\r\n"),
        "{shown}"
    );
    drop(keep);
}

#[test]
fn the_prompt_on_a_terminal_ends_with_the_message_and_the_block() {
    let sandbox = Sandbox::new();
    let (master, slave) = terminal();
    let keep = slave.try_clone().expect("the slave end is duplicated");
    type_ahead(&master, b"k\nsecret\n");
    let mut command = sandbox.command(&["encrypt_string", "-p", PW[0], PW[1]]);
    command
        .stdin(Stdio::from(slave.try_clone().expect("duplicated")))
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped());
    let child = command.spawn().expect("volant starts");
    drop(command);
    let (code, stderr, _) = finish(child);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.ends_with("Encryption successful\n"), "{stderr}");
    let shown = screen(&master);
    assert!(
        shown.contains("Variable name (enter for no name): "),
        "{shown}"
    );
    assert!(
        shown.contains("k: !vault |\r\n          $ANSIBLE_VAULT;1.1;AES256\r\n"),
        "{shown}"
    );
    drop(keep);
}

/// What the reference prints for the same strings has the same shape, and each opens under the
/// other tool.
#[test]
fn a_block_one_tool_wrote_the_other_opens() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let sandbox = Sandbox::new();
    let run = |args: &[&str]| {
        Command::new(&python)
            .args([
                "-c",
                "from ansible.cli.vault import VaultCLI; VaultCLI.cli_executor()",
            ])
            .args(args)
            .current_dir(&sandbox.dir)
            .stdin(Stdio::null())
            .output()
            .expect("the reference starts")
    };
    let theirs = run(&["encrypt_string", PW[0], PW[1], "-n", "k", "foobar"]);
    assert_eq!(theirs.status.code(), Some(0), "{}", text(&theirs.stderr));
    let ours = sandbox.run(&["encrypt_string", PW[0], PW[1], "-n", "k", "foobar"], None);
    let (theirs, ours) = (text(&theirs.stdout), text(&ours.stdout));
    let shape = |s: &str| s.lines().map(str::len).collect::<Vec<_>>();
    assert_eq!(shape(&theirs), shape(&ours));
    assert_eq!(sandbox.reveal(&blocks(&theirs)[0].1), b"foobar");
    std::fs::write(sandbox.dir.join("mine"), &blocks(&ours)[0].1).expect("a block");
    let decrypted = run(&["decrypt", "mine", "--output", "-", PW[0], PW[1]]);
    assert_eq!(decrypted.stdout, b"foobar");
}
