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
            .env_remove("PAGER");
        command
    }

    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Output {
        process::run_within(&mut self.command(args), DEADLINE, stdin)
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
