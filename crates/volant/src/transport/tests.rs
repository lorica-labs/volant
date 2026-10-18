// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::Path;
use std::time::Duration;

use rstest::rstest;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(unix)]
use super::bootstrap::is_this_agent;
use super::bootstrap::{
    Captured, EXIT_AGENT_MISSING, EXIT_AGENT_UNRUNNABLE, EXIT_SSH_FAILURE, EXIT_UNAME_FAILED,
    bootstrap_probe_ran,
};
use super::control::{CONTROL_PATH_MAX, asks_for_debug};
#[cfg(unix)]
use super::control::{STOP_MASTER_TIMEOUT, control_dir_in};
use super::escalation::{
    SudoForm, agent_version_ran, become_success, escalated_command, escalation_outcome,
    interactive, local_argv, preamble, settle_form, su_command, su_ended, su_exchange, sudo_prefix,
    unreachable_message,
};
use super::shell::shell_word;
use super::*;
use crate::inventory::Host;
use crate::testing::{defaults, scratch};

fn host(vars: Value) -> Host {
    Host {
        name: "web1".into(),
        vars: vars
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

fn escalation(password: Option<&str>) -> Escalation {
    Escalation {
        method: BecomeMethod::Sudo,
        user: "deploy".into(),
        password: password.map(str::to_string),
        exe: Vec::new(),
        flags: None,
    }
}

/// The sudo command line, the stdin preamble and the remote prefix follow the probed form.
#[test]
fn the_sudo_words_follow_the_probed_form() {
    {
        let password = format!("only-this-run-{}", std::process::id());
        let secret = escalation(Some(password.as_str()));
        let argv = local_argv("/usr/bin/volant-agent", Some((&secret, SudoForm::NoPrompt)));
        assert_eq!(
            argv,
            [
                "sudo",
                "-H",
                "-n",
                "-u",
                "deploy",
                "--",
                "/usr/bin/volant-agent"
            ]
        );
        let argv = local_argv(
            "/usr/bin/volant-agent",
            Some((&secret, SudoForm::ReadStdin)),
        );
        assert_eq!(
            argv,
            [
                "sudo",
                "-H",
                "-k",
                "-S",
                "-p",
                "",
                "-u",
                "deploy",
                "--",
                "/usr/bin/volant-agent"
            ]
        );
        assert!(
            !argv.contains(&password),
            "the password never reaches a command line: {argv:?}"
        );
        assert_eq!(
            local_argv("/usr/bin/volant-agent", None),
            ["/usr/bin/volant-agent"],
            "without escalation the agent is the command"
        );
    }
    {
        let password = format!("only-this-run-{}", std::process::id());
        let secret = escalation(Some(password.as_str()));
        assert_eq!(
            preamble(Some((&secret, SudoForm::ReadStdin))),
            Some(format!("{password}\n").into_bytes())
        );
        assert_eq!(preamble(Some((&secret, SudoForm::NoPrompt))), None);
        assert_eq!(
            preamble(Some((&escalation(None), SudoForm::ReadStdin))),
            None
        );
        assert_eq!(preamble(None), None);
    }
    {
        let password = format!("only-this-run-{}", std::process::id());
        let secret = escalation(Some(password.as_str()));
        let prefix = sudo_prefix(Some((&secret, SudoForm::ReadStdin)));
        assert_eq!(prefix, "'sudo' '-H' '-k' '-S' '-p' '' -u 'deploy' -- ");
        assert!(!prefix.contains(&password), "{prefix}");
        assert_eq!(
            sudo_prefix(Some((
                &Escalation {
                    user: "it's me".into(),
                    ..escalation(None)
                },
                SudoForm::NoPrompt
            ))),
            r"'sudo' '-H' '-n' -u 'it'\''s me' -- ",
            "a user name that would otherwise break out of the remote shell word is quoted"
        );
        assert!(sudo_prefix(None).is_empty());
    }
}

/// `become_exe` replaces `sudo`, `become_flags` replaces `-H`, and the form that writes a
/// password takes `-n` back out of them as sudo.py does (scout case `c4`'s flags).
#[rstest]
#[case::quiet(
    SudoForm::NoPrompt,
    "'/usr/bin/sudo' '-H' '-S' '-n' '-n' -u 'deploy' -- "
)]
#[case::asking(
    SudoForm::ReadStdin,
    "'/usr/bin/sudo' '-H' '-S' '-k' '-S' '-p' '' -u 'deploy' -- "
)]
fn become_exe_and_become_flags_shape_the_sudo_words(#[case] form: SudoForm, #[case] want: &str) {
    let e = Escalation {
        exe: vec!["/usr/bin/sudo".into()],
        flags: Some(vec!["-H".into(), "-S".into(), "-n".into()]),
        ..escalation(Some("pw"))
    };
    assert_eq!(sudo_prefix(Some((&e, form))), want);
    for (flag, kept) in [
        ("-n", None),
        ("--non-interactive", None),
        ("-Hn", Some("-H")),
        ("-nH", Some("-H")),
        ("-Snn", Some("-Sn")),
        ("-E", Some("-E")),
        ("--preserve-env", Some("--preserve-env")),
    ] {
        assert_eq!(interactive(flag).as_deref(), kept, "{flag}");
    }
}

/// `su <flags> <user> -c '/bin/sh -c ...'`, every word quoted, the terminal made raw before
/// the line that says `su` let the command in.
#[test]
fn the_su_command_is_the_references_with_a_raw_terminal() {
    let e = Escalation {
        method: BecomeMethod::Su,
        user: "root".into(),
        ..escalation(None)
    };
    assert_eq!(
        su_command(&e, "uname -m", BECOME_SUCCESS),
        r"exec 'su' 'root' -c '/bin/sh -c '\''stty raw -echo -iexten || exit 1; echo MARK-42; uname -m'\'''"
    );
    let e = Escalation {
        exe: vec!["/bin/su".into()],
        flags: Some(vec!["-".into()]),
        ..e
    };
    assert!(
        su_command(&e, ":", BECOME_SUCCESS).starts_with("exec '/bin/su' '-' 'root' -c "),
        "{}",
        su_command(&e, ":", BECOME_SUCCESS)
    );
    assert_ne!(
        become_success(),
        become_success(),
        "a fresh line every time"
    );
    assert_ne!(
        e.program(),
        escalation(None).program(),
        "su and sudo to one user are two links"
    );
}

/// The success line the fakes below print.
const BECOME_SUCCESS: &str = "MARK-42";

/// What a fake `su` on the other end of [`su_exchange`] does.
#[derive(Clone, Copy, Debug)]
enum FakeSu {
    Accepts,
    Refuses,
    AsksTwice,
    WaitsForever,
    IsGone,
    EchoesAndDies,
    Localised,
    Announces,
}

/// Plays `su` over `io` for one exchange; `pw` is the password it accepts.
async fn fake_su(mode: FakeSu, io: tokio::io::DuplexStream, pw: &str) {
    use tokio::io::AsyncBufReadExt;
    let (read, mut write) = tokio::io::split(io);
    let mut lines = tokio::io::BufReader::new(read).lines();
    let prompt = if matches!(mode, FakeSu::Localised) {
        "Mot de passe : "
    } else {
        "Password: "
    };
    if matches!(mode, FakeSu::IsGone) {
        let _ = write.write_all(b"su: user nosuch does not exist\r\n").await;
        return;
    }
    let _ = write.write_all(prompt.as_bytes()).await;
    if matches!(mode, FakeSu::WaitsForever) {
        std::future::pending::<()>().await;
    }
    let answer = lines.next_line().await.ok().flatten().unwrap_or_default();
    let reply = match mode {
        FakeSu::EchoesAndDies => format!("{answer}\r\nsu: something odd\r\n"),
        FakeSu::Refuses => "\r\nsu: Authentication failure\r\n".to_string(),
        FakeSu::AsksTwice => format!("\r\n{prompt}"),
        // util-linux 2.41's session announcement, with no line end before the marker.
        FakeSu::Announces => {
            format!("\r\n\x1b]3008;start=1;comm=su\x1b\\{BECOME_SUCCESS}\nframes")
        }
        _ if answer == pw => format!("\r\n{BECOME_SUCCESS}\nframes"),
        _ => "\r\nsu: Authentication failure\r\n".to_string(),
    };
    let _ = write.write_all(reply.as_bytes()).await;
    if matches!(mode, FakeSu::AsksTwice) {
        std::future::pending::<()>().await;
    }
}

/// A prompt is answered once, the command's first byte is left on the stream, and every way
/// `su` can say no comes back as the reference words it, without the password in it.
#[rstest]
#[case::accepts(FakeSu::Accepts, Some("pw"), Ok(None))]
#[case::localised(FakeSu::Localised, Some("pw"), Ok(None))]
#[case::announces(FakeSu::Announces, Some("pw"), Ok(None))]
#[case::refuses(FakeSu::Refuses, Some("pw"), Err(INCORRECT_SU_PASSWORD))]
#[case::wrong(FakeSu::Accepts, Some("nope"), Err(INCORRECT_SU_PASSWORD))]
#[case::asks_twice(FakeSu::AsksTwice, Some("pw"), Err(INCORRECT_SU_PASSWORD))]
#[case::no_password(
    FakeSu::Accepts,
    None,
    Err("Task failed: Timeout (0s) waiting for privilege escalation prompt:")
)]
#[case::no_prompt(
    FakeSu::WaitsForever,
    Some("pw"),
    Err("Task failed: Timeout (0s) waiting for privilege escalation prompt:")
)]
#[case::gone(FakeSu::IsGone, Some("pw"), Ok(Some("su: user nosuch does not exist")))]
#[case::echoes(FakeSu::EchoesAndDies, Some("pw"), Ok(Some("<redacted>")))]
#[tokio::test]
async fn su_is_talked_through_as_the_reference_does(
    #[case] mode: FakeSu,
    #[case] password: Option<&str>,
    #[case] want: Result<Option<&str>, &str>,
) {
    let (mut ours, theirs) = tokio::io::duplex(1024);
    let su = tokio::spawn(fake_su(mode, theirs, "pw"));
    let (mut read, mut write) = tokio::io::split(&mut ours);
    let got = su_exchange(
        &mut read,
        &mut write,
        password,
        BECOME_SUCCESS,
        Duration::from_millis(300),
    )
    .await;
    match (got, want) {
        (Ok(None), Ok(None)) => {
            let mut rest = [0u8; 6];
            read.read_exact(&mut rest).await.unwrap();
            assert_eq!(
                &rest, b"frames",
                "the command's own bytes stay on the stream"
            );
        }
        (Ok(Some(said)), Ok(Some(says))) => {
            assert!(said.contains(says), "{said}");
            assert!(
                !said.contains("pw\r"),
                "the echoed password is taken out: {said}"
            );
        }
        (Err(err), Err(says)) => assert_eq!(err.to_string(), says),
        (got, want) => panic!("got {got:?}, wanted {want:?}"),
    }
    su.abort();
}

/// The stream ending before `su` let the command in: `ssh`'s failure when it exited 255,
/// `su`'s own words otherwise.
#[test]
fn a_su_that_ends_early_is_told_apart_by_the_exit_code() {
    assert!(matches!(
        su_ended(Some(EXIT_SSH_FAILURE), "", "Connection refused", None),
        ConnectError::Unreachable(msg) if msg.ends_with("Connection refused")
    ));
    assert!(matches!(
        su_ended(Some(1), "su: user x does not exist", "", None),
        ConnectError::Become(msg) if msg == "su: user x does not exist"
    ));
    assert!(matches!(
        su_ended(Some(1), " \r\n", "", None),
        ConnectError::Become(msg) if msg == "su exited without starting the command"
    ));
}

/// The strings come from the two `sudo` implementations measured for this: `sudo` 1.9.15p5 on
/// the target machine and `sudo-rs` 0.2.13 on the development machine.
#[test]
fn sudos_refusals_are_told_apart_by_whether_a_password_was_offered() {
    let outcome = |stderr: &str, offered| match escalation_outcome(false, stderr, offered) {
        Err(ConnectError::Become(msg)) => msg,
        other => panic!("expected Become, got {other:?}"),
    };
    let password = format!("only-this-run-{}", std::process::id());
    for stderr in [
        "sudo: a password is required",
        "sudo: interactive authentication is required",
    ] {
        assert_eq!(outcome(stderr, None), MISSING_SUDO_PASSWORD, "{stderr}");
    }
    for stderr in [
        "Sorry, try again.\n\nsudo: no password was provided",
        "sudo: Authentication failed, try again.",
        "sudo: Authentication required but not attempted",
        "sudo: 1 incorrect password attempt",
    ] {
        assert_eq!(
            outcome(stderr, Some(password.as_str())),
            INCORRECT_SUDO_PASSWORD,
            "{stderr}"
        );
    }
    assert_eq!(
        outcome("sh: 1: sudo: not found", None),
        "sh: 1: sudo: not found",
        "a host without sudo gets the shell's own words, not a guess about passwords"
    );
    assert_eq!(
        outcome("", None),
        "sudo refused without a message",
        "a silent refusal still says something"
    );
}

/// A refusal's own words reach a task result, and the password the controller offered never goes with them.
#[rstest]
#[case::sudo(|stderr: &str, password: &str| match escalation_outcome(false, stderr, Some(password)) {
    Err(ConnectError::Become(msg)) => msg,
    other => panic!("expected Become, got {other:?}"),
}, "sudo: bespoke build read '{password}' and disliked it", "disliked it")]
#[case::ssh(
    |stderr: &str, password: &str| unreachable_message(stderr, Some(password)),
    "client_loop: send disconnect: Broken pipe reading '{password}'",
    "Broken pipe",
)]
fn a_message_never_carries_the_password_back(
    #[case] message: fn(&str, &str) -> String,
    #[case] stderr: &str,
    #[case] survives: &str,
) {
    let password = format!("only-this-run-{}", std::process::id());
    let msg = message(&stderr.replace("{password}", &password), &password);
    // Neither assertion prints `msg`: a failure would be one more place carrying the password.
    assert!(!msg.contains(&password));
    assert!(msg.contains("<redacted>") && msg.contains(survives));
}
fn agent_version() -> Captured {
    Captured {
        code: Some(0),
        stdout: format!("volant-agent {}", env!("CARGO_PKG_VERSION")),
        stderr: String::new(),
    }
}

fn refused(stderr: &str) -> Captured {
    Captured {
        code: Some(1),
        stdout: String::new(),
        stderr: stderr.to_string(),
    }
}

/// The `sudo` form is settled by asking `sudo`, not by whether a password happens to be
/// available.
#[tokio::test]
async fn the_probe_settles_the_form_by_asking_sudo_before_offering_anything() {
    use std::cell::RefCell;
    use std::future::ready;

    let password = format!("only-this-run-{}", std::process::id());
    let with = escalation(Some(password.as_str()));
    let without = escalation(None);

    let asked = RefCell::new(Vec::new());
    let (form, _) = settle_form(&with, agent_version_ran, |form| {
        asked.borrow_mut().push(form);
        ready(Ok(agent_version()))
    })
    .await
    .expect("a sudo that answers -n escalates");
    assert_eq!(form, SudoForm::NoPrompt);
    assert_eq!(
        *asked.borrow(),
        [SudoForm::NoPrompt],
        "a sudo that needs no authentication is never asked a second time"
    );
    assert_eq!(
        preamble(Some((&with, form))),
        None,
        "and is never written a password it would leave on the pipe"
    );

    let asked = RefCell::new(Vec::new());
    let (form, _) = settle_form(&with, agent_version_ran, |form| {
        asked.borrow_mut().push(form);
        ready(Ok(match form {
            SudoForm::NoPrompt => refused("sudo: a password is required"),
            SudoForm::ReadStdin => agent_version(),
        }))
    })
    .await
    .expect("the password answers the refusal");
    assert_eq!(form, SudoForm::ReadStdin);
    assert_eq!(*asked.borrow(), [SudoForm::NoPrompt, SudoForm::ReadStdin]);
    assert!(preamble(Some((&with, form))).is_some());

    let asked = RefCell::new(Vec::new());
    let err = settle_form(&without, agent_version_ran, |form| {
        asked.borrow_mut().push(form);
        ready(Ok(refused("sudo: a password is required")))
    })
    .await
    .expect_err("no password to offer");
    assert_eq!(err.to_string(), MISSING_SUDO_PASSWORD);
    assert_eq!(
        *asked.borrow(),
        [SudoForm::NoPrompt],
        "with nothing to offer there is nothing to try twice"
    );

    let err = settle_form(&with, agent_version_ran, |form| {
        ready(Ok(match form {
            SudoForm::NoPrompt => refused("sudo: a password is required"),
            SudoForm::ReadStdin => refused("sudo: 1 incorrect password attempt"),
        }))
    })
    .await
    .expect_err("the password was wrong");
    assert_eq!(err.to_string(), INCORRECT_SUDO_PASSWORD);

    let asked = RefCell::new(Vec::new());
    let err = settle_form(&with, agent_version_ran, |form| {
        asked.borrow_mut().push(form);
        ready(Ok(refused("sh: 1: sudo: not found")))
    })
    .await
    .expect_err("no sudo at all");
    assert_eq!(err.to_string(), "sh: 1: sudo: not found");
    assert_eq!(
        *asked.borrow(),
        [SudoForm::NoPrompt],
        "a refusal no password can answer is final"
    );
}

/// A `sudo` that exits zero having run something else is not an escalation that worked.
#[test]
fn local_escalation_succeeds_only_on_the_agents_own_version_line() {
    let printed = |code, stdout: &str| {
        agent_version_ran(&Captured {
            code,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    };
    let version = format!("volant-agent {}", env!("CARGO_PKG_VERSION"));
    assert!(printed(Some(0), &version));
    assert!(
        printed(Some(0), &format!("{version}\n")),
        "a trailing newline is not a different version"
    );
    assert!(
        !printed(Some(0), "volant-agent 0.0.0-stale"),
        "another version is not this agent"
    );
    assert!(
        !printed(Some(0), ""),
        "a zero exit with nothing printed proves nothing"
    );
    assert!(!printed(Some(1), &version), "a refusal is a refusal");
}

/// The remote probe cannot ask for a version, because it is the upload of that very agent it
/// settles the `sudo` form for.
#[test]
fn the_remote_escalation_probe_is_proved_by_the_bootstraps_own_exit_codes() {
    let exited = |code| {
        bootstrap_probe_ran(&Captured {
            code,
            stdout: String::new(),
            stderr: String::new(),
        })
    };
    for code in [
        0,
        EXIT_AGENT_MISSING,
        EXIT_AGENT_UNRUNNABLE,
        EXIT_UNAME_FAILED,
    ] {
        assert!(exited(Some(code)), "the probe script's own code {code}");
    }
    for code in [1, 126, 127, EXIT_SSH_FAILURE] {
        assert!(
            !exited(Some(code)),
            "{code} belongs to sudo, the shell or ssh, not to the probe"
        );
    }
    assert!(!exited(None), "a command killed by a signal proves nothing");
}

/// An escalated bootstrap command goes to an inner `sh -c`, so the `~` in `remote_tmp` names
/// the escalated user's home.
#[test]
fn an_escalated_bootstrap_command_is_expanded_by_a_shell_running_as_that_user() {
    let secret = escalation(None);
    let plain = escalated_command(None, "d=~/'.ansible/tmp'; mkdir -p \"$d\"");
    assert_eq!(
        plain, "d=~/'.ansible/tmp'; mkdir -p \"$d\"",
        "a link that does not escalate is not wrapped at all"
    );
    let wrapped = escalated_command(
        Some((&secret, SudoForm::NoPrompt)),
        "d=~/'.ansible/tmp'; mkdir -p \"$d\"",
    );
    assert_eq!(
        wrapped,
        r#"'sudo' '-H' '-n' -u 'deploy' -- sh -c 'd=~/'\''.ansible/tmp'\''; mkdir -p "$d"'"#
    );
    // Run the wrapping against a real `sh`, with the `sudo` words dropped: the failure this
    // guards against is a quoting one, and what has to hold is that the tilde reaches the
    // inner shell unexpanded, so the escalated user's own `HOME` is what it names. The
    // outer shell stands in for the remote one and the inner for the one `sudo` starts.
    #[cfg(unix)]
    {
        let inner = format!("printf %s {}", shell_word("~/a b/c"));
        let wrapped = escalated_command(Some((&secret, SudoForm::NoPrompt)), &inner);
        let (_, sh) = wrapped
            .split_once("-- ")
            .expect("the sudo words end with --");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(sh)
            .env("HOME", "/tmp/volant home")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "/tmp/volant home/a b/c",
            "the tilde expands where it should and the rest stays one word"
        );
    }
}

#[test]
fn ssh_is_the_default_connection_and_reads_the_ansible_variables() {
    let t = Transport::for_host(
        &host(json!({
            "ansible_host": "10.0.0.5", "ansible_port": 2222, "ansible_user": "deploy",
            "ansible_ssh_private_key_file": "/keys/id", "ansible_ssh_common_args": "-o ProxyJump=bastion",
            "ansible_ssh_extra_args": "-vv", "ansible_remote_tmp": "/var/tmp/v"
        })),
        &defaults(),
    )
    .unwrap();
    let Transport::Ssh(target) = t else {
        panic!("expected ssh")
    };
    let argv = target.ssh_argv("true");
    let text = argv.join(" ");
    assert!(
        text.starts_with("ssh -o BatchMode=yes -o ConnectTimeout=10"),
        "{text}"
    );
    assert!(
        text.contains("-p 2222") && text.contains("-i /keys/id") && text.contains("-l deploy"),
        "{text}"
    );
    assert!(
        text.contains("-o ProxyJump=bastion -vv"),
        "common args come before extra args: {text}"
    );
    assert!(text.ends_with("10.0.0.5 -- true"), "{text}");
    assert!(
        !text.contains("StrictHostKeyChecking"),
        "checking on means ssh defaults: {text}"
    );
    assert_eq!(target.remote_tmp, "/var/tmp/v");
}

/// `run_capturing` builds its command line with this exact call, so the assertions below
/// are about the production path and not about a vector the test filled in itself.
#[test]
fn compression_is_an_option_not_an_argument_of_the_remote_command() {
    let Transport::Ssh(target) = Transport::for_host(&host(json!({})), &defaults()).unwrap() else {
        panic!()
    };
    let argv = target.ssh_argv_with("cat > f", true);
    assert_eq!(argv[1], "-C", "{argv:?}");
    assert!(
        argv.join(" ").ends_with("web1 -- cat > f"),
        "nothing may follow the remote command: {argv:?}"
    );
    assert!(
        !target
            .ssh_argv_with("true", false)
            .contains(&"-C".to_string()),
        "a command with no payload is not compressed"
    );
}

#[test]
fn the_bootstrap_probe_and_upload_commands_are_shaped_for_a_posix_shell() {
    let Transport::Ssh(target) = Transport::for_host(&host(json!({})), &defaults()).unwrap() else {
        panic!()
    };
    let probe = target.probe_command();
    assert!(
        probe.contains(&format!("volant-agent-{}", env!("CARGO_PKG_VERSION"))),
        "{probe}"
    );
    assert!(
        probe.contains("--version") && probe.contains("uname -m") && probe.contains("exit 42"),
        "{probe}"
    );
    assert!(
        probe.contains("exit 45"),
        "a cached agent that cannot run has its own code, so its status never reaches ssh: {probe}"
    );
    assert!(
        probe.contains("uname -m || exit 46"),
        "a uname that fails must not be read as an architecture: {probe}"
    );
    let upload = target.upload_command(1_000_000);
    assert!(
        upload.contains("df -Pk") && upload.contains("exit 43"),
        "{upload}"
    );
    assert!(
        upload.contains(&format!("-ge {}", space_needed_kib(1_000_000))),
        "{upload}"
    );
    assert!(
        upload.contains("chmod 755") && upload.contains("mv "),
        "{upload}"
    );
    assert!(
        upload.contains("-eq 1000000") && upload.contains("exit 44"),
        "a transfer cut short must not reach the final name: {upload}"
    );
    assert!(
        upload.find("wc -c").unwrap() < upload.find("chmod 755").unwrap(),
        "the size is checked before the file becomes executable: {upload}"
    );
    assert!(
        upload.contains("-name 'volant-agent.tmp.*' -mmin +60 -delete"),
        "an orphaned temporary from a killed connection is swept, never a live sibling: {upload}"
    );
    assert!(
        upload.find("mv ").unwrap() < upload.find("-mmin +60").unwrap(),
        "the sweep runs after this upload's own file has already been renamed away: {upload}"
    );
}

/// A `remote_tmp` holding a space survives a real `sh`, and a cached agent exiting 255 is
/// reported as 45, not as an ssh failure.
#[cfg(unix)]
#[test]
fn a_remote_tmp_with_a_space_survives_the_shell() {
    use std::io::Write;
    use std::process::{Command as SyncCommand, Stdio};

    let guard = scratch();
    let base = guard.path().join("volant quoting");
    std::fs::create_dir_all(&base).unwrap();
    let Transport::Ssh(target) = Transport::for_host(
        &host(json!({ "ansible_remote_tmp": base.to_str().unwrap() })),
        &defaults(),
    )
    .unwrap() else {
        panic!()
    };

    let payload = b"#!/bin/sh\nexit 255\n";
    let mut child = SyncCommand::new("sh")
        .arg("-c")
        .arg(target.upload_command(payload.len() as u64))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    let upload = child.wait_with_output().unwrap();
    assert_eq!(
        upload.status.code(),
        Some(0),
        "upload: {}",
        String::from_utf8_lossy(&upload.stderr)
    );
    let landed = base
        .join(format!("volant-agent-{}", env!("CARGO_PKG_VERSION")))
        .join("volant-agent");
    assert_eq!(std::fs::read(&landed).unwrap(), payload, "{landed:?}");

    let probe = SyncCommand::new("sh")
        .arg("-c")
        .arg(target.probe_command())
        .output()
        .unwrap();
    assert_eq!(
        probe.status.code(),
        Some(EXIT_AGENT_UNRUNNABLE),
        "a cached agent exiting 255 must not look like a connection failure: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
}

/// A cached agent is reused only when it is the very binary this controller would upload.
#[cfg(unix)]
#[test]
fn a_cached_agent_of_this_version_but_other_bytes_is_not_reused() {
    use std::io::Write;
    use std::process::{Command as SyncCommand, Stdio};

    let exe = std::env::current_exe().expect("this test's own binary path");
    let local = exe
        .parent()
        .and_then(Path::parent)
        .expect("deps/ has a parent")
        .join("volant-agent");
    let ours = std::fs::read(&local).expect("the agent is built beside the tests");
    let ours_hash = blake3::hash(&ours).to_hex().to_string();
    let guard = scratch();
    let base = guard.path().to_path_buf();
    std::fs::create_dir_all(&base).unwrap();
    let Transport::Ssh(target) = Transport::for_host(
        &host(json!({ "ansible_remote_tmp": base.to_str().unwrap() })),
        &defaults(),
    )
    .unwrap() else {
        panic!()
    };
    let cache_then_probe = |payload: &[u8]| {
        let mut child = SyncCommand::new("sh")
            .arg("-c")
            .arg(target.upload_command(payload.len() as u64))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(payload).unwrap();
        let upload = child.wait_with_output().unwrap();
        assert!(upload.status.success(), "{upload:?}");
        let probe = SyncCommand::new("sh")
            .arg("-c")
            .arg(target.probe_command())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        Captured {
            code: probe.status.code(),
            stdout: String::from_utf8_lossy(&probe.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&probe.stderr).into_owned(),
        }
    };

    let mut instrumented = ours.clone();
    instrumented.extend_from_slice(b"one more section");
    let probe = cache_then_probe(&instrumented);
    assert_eq!(probe.code, Some(0), "the other build runs: {probe:?}");
    assert!(
        !is_this_agent(&probe, &ours_hash),
        "a build of this version with other bytes is not this agent: {probe:?}"
    );

    let script = format!(
        "#!/bin/sh\necho volant-agent {}\n",
        env!("CARGO_PKG_VERSION")
    );
    let probe = cache_then_probe(script.as_bytes());
    assert!(
        !is_this_agent(&probe, &ours_hash),
        "a script printing this version is not this agent: {probe:?}"
    );

    let probe = cache_then_probe(&ours);
    assert!(
        is_this_agent(&probe, &ours_hash),
        "the controller's own agent is reused: {probe:?}"
    );
}

/// Tilde users, tilde words, agent triples and the headroom of an upload.
#[test]
fn the_small_remote_rules_hold() {
    {
        for value in ["~", "~root", "~some.user-1"] {
            assert!(validate_remote_tmp(value).is_ok(), "{value}");
        }
    }
    {
        assert_eq!(shell_word("~/a b/c"), "~/'a b/c'");
        assert_eq!(shell_word("~"), "~");
        assert_eq!(
            shell_word("~deploy/a b"),
            "~deploy/'a b'",
            "a named user's home expands too, not just the caller's own"
        );
        assert_eq!(shell_word("/var/tmp/a b"), "'/var/tmp/a b'");
        assert_eq!(
            shell_word("~/it's here"),
            r"~/'it'\''s here'",
            "an embedded single quote in the quoted remainder is escaped"
        );
    }
    {
        assert_eq!(triple_for("x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(triple_for("aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(triple_for("arm64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(triple_for("riscv64"), None);
    }
    {
        // 672480 bytes is 656.7 KiB; two copies are 1313 KiB, plus a mebibyte of headroom.
        assert_eq!(space_needed_kib(672_480), 2337);
        assert_eq!(space_needed_kib(0), 1024, "the headroom stands on its own");
        assert_eq!(
            space_needed_kib(1_048_576),
            3072,
            "a mebibyte agent needs three"
        );
        assert!(
            space_needed_kib(5_000_000) > 2 * 5_000_000 / 1024,
            "always more than the two copies it covers"
        );
    }
}

/// Every `ansible_*` name this file reads is on the connections page, and the page lists no other.
#[test]
fn the_published_host_variables_are_the_ones_this_module_reads() {
    let source = [
        include_str!("bootstrap.rs"),
        include_str!("choose.rs"),
        include_str!("control.rs"),
        include_str!("defaults.rs"),
        include_str!("escalation/mod.rs"),
        include_str!("escalation/probe.rs"),
        include_str!("escalation/su.rs"),
        include_str!("escalation/sudo.rs"),
        include_str!("session.rs"),
        include_str!("shell.rs"),
        include_str!("ssh.rs"),
        include_str!("mod.rs"),
    ]
    .concat();
    let mut read: Vec<&str> = source
        .split("\"ansible_")
        .skip(1)
        .filter_map(|rest| rest.split_once('"').map(|(name, _)| name))
        .filter(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .collect();
    read.sort_unstable();
    read.dedup();

    let page = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/src/content/docs/hosts/connections.md"),
    )
    .expect("the connections page is in the repository");
    let mut published: Vec<&str> = page
        .lines()
        .filter_map(|line| line.strip_prefix("| `ansible_"))
        .filter_map(|rest| rest.split_once('`').map(|(name, _)| name))
        .filter(|name| !name.starts_with("become"))
        .collect();
    published.sort_unstable();
    published.dedup();
    assert_eq!(
        published, read,
        "the connections page and Transport::for_vars disagree about the host variables"
    );
}

/// What the host says, or the defaults when it says nothing, shapes the ssh command line.
#[rstest]
#[case::host_key_checking_off(json!({}), |d: &mut ConnectionDefaults| d.host_key_checking = false,
    &["-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"], None)]
#[case::defaults_apply(json!({}), |d: &mut ConnectionDefaults| {
    d.remote_user = Some("ops".into());
    d.private_key = Some("/k".into());
}, &["-l ops", "-i /k"], None)]
#[case::host_beats_defaults(json!({"ansible_user": "me"}), |d: &mut ConnectionDefaults| d.remote_user = Some("ops".into()), &["-l me"], None)]
#[case::quoted_port(json!({"ansible_port": "2222"}), |_: &mut ConnectionDefaults| {}, &["-p 2222"], Some(2222))]
fn the_host_and_the_defaults_shape_the_ssh_command(
    #[case] vars: Value,
    #[case] tweak: fn(&mut ConnectionDefaults),
    #[case] fragments: &[&str],
    #[case] port: Option<u16>,
) {
    let mut d = defaults();
    tweak(&mut d);
    let Transport::Ssh(target) = Transport::for_host(&host(vars), &d).unwrap() else {
        panic!("expected ssh")
    };
    let text = target.ssh_argv("true").join(" ");
    for fragment in fragments {
        assert!(text.contains(fragment), "{fragment}: {text}");
    }
    assert_eq!(target.port, port);
    assert!(
        text.ends_with("web1 -- true"),
        "the inventory name is the address by default: {text}"
    );
}

/// The order measured on ansible-core 2.19.12: a host variable beats the task's keyword,
/// which beats the command line and `ansible.cfg`.
#[rstest]
#[case::keyword_over_cli(json!({}), Some("local"), Some("ssh"), "local")]
#[case::host_over_keyword(json!({"ansible_connection": "ssh"}), Some("local"), None, "ssh")]
#[case::cli_alone(json!({}), None, Some("local"), "local")]
#[case::keyword_alone(json!({}), Some("local"), None, "local")]
#[case::unknown_keyword(json!({}), Some("nosuch"), None, "the connection plugin 'nosuch' was not found")]
#[case::unknown_cli(json!({}), None, Some("nosuch"), "the connection plugin 'nosuch' was not found")]
fn the_connection_keyword_ranks_between_the_host_and_the_command_line(
    #[case] vars: Value,
    #[case] keyword: Option<&str>,
    #[case] cli: Option<&str>,
    #[case] want: &str,
) {
    let mut d = defaults();
    d.connection = cli.map(str::to_string);
    let keywords = ConnectionKeywords {
        connection: keyword.map(str::to_string),
        ..ConnectionKeywords::default()
    };
    let got = match Transport::for_task("h1", vars.as_object().unwrap(), &d, &keywords) {
        Ok(Transport::Local) => "local".to_string(),
        Ok(Transport::Ssh(_)) => "ssh".to_string(),
        Err(err) => err.to_string(),
    };
    assert_eq!(got, want);
}

/// Same order for the user and the port.
#[rstest]
#[case::keyword_over_cli(json!({}), Some("task"), Some(2222), Some("ops"), "task", Some(2222))]
#[case::host_over_keyword(json!({"ansible_user": "host", "ansible_port": 22}), Some("task"), Some(2222), Some("ops"), "host", Some(22))]
#[case::cli_alone(json!({}), None, None, Some("ops"), "ops", None)]
fn the_user_and_port_keywords_rank_between_the_host_and_the_command_line(
    #[case] vars: Value,
    #[case] user: Option<&str>,
    #[case] port: Option<u16>,
    #[case] cli_user: Option<&str>,
    #[case] want_user: &str,
    #[case] want_port: Option<u16>,
) {
    let mut d = defaults();
    d.remote_user = cli_user.map(str::to_string);
    let keywords = ConnectionKeywords {
        user: user.map(str::to_string),
        port,
        connection: None,
    };
    let Transport::Ssh(target) =
        Transport::for_task("h1", vars.as_object().unwrap(), &d, &keywords).unwrap()
    else {
        panic!("expected ssh")
    };
    assert_eq!(
        (target.user.as_deref(), target.port),
        (Some(want_user), want_port)
    );
}

/// A managed host cannot choose how it is reached: facts named like the connection settings
/// are dropped, and the inventory's answer stands.
#[rstest]
#[case::user("ansible_user", json!("evil"))]
#[case::port("ansible_port", json!(2))]
#[case::connection("ansible_connection", json!("local"))]
fn facts_cannot_pick_the_user_port_or_connection(#[case] name: &str, #[case] value: Value) {
    let inventory = crate::inventory::Inventory::parse_ini(
        "h1 ansible_host=real ansible_user=ops ansible_port=2200\n",
    )
    .unwrap();
    let mut store =
        crate::vars::VarStore::new(&inventory, None, Path::new("."), Map::new()).unwrap();
    let mut facts = Map::new();
    facts.insert(name.to_string(), value);
    assert_eq!(store.gather_facts("h1", &facts), [name]);
    let vars = store.for_host("h1", &crate::vars::Scope::default());
    let keywords = ConnectionKeywords::default();
    let Transport::Ssh(target) = Transport::for_task("h1", &vars, &defaults(), &keywords).unwrap()
    else {
        panic!("a fact turned the connection into something else")
    };
    assert_eq!(
        (target.address.as_str(), target.user.as_deref(), target.port),
        ("real", Some("ops"), Some(2200))
    );
}

/// A blank `ansible_remote_tmp` reads as no value; one with spaces around it is trimmed.
#[rstest]
#[case("", "~/.ansible/tmp")]
#[case("   ", "~/.ansible/tmp")]
#[case("  /var/tmp/v  ", "/var/tmp/v")]
fn a_host_remote_tmp_is_trimmed_and_a_blank_is_no_value(#[case] value: &str, #[case] want: &str) {
    let Transport::Ssh(target) =
        Transport::for_host(&host(json!({ "ansible_remote_tmp": value })), &defaults()).unwrap()
    else {
        panic!("expected ssh")
    };
    assert_eq!(target.remote_tmp, want);
}

/// A host the engine cannot connect to is refused with the variable or the connection named.
#[rstest]
#[case(
    "ansible_ssh_common_args",
    "-o ProxyCommand='ssh bastion",
    "ProxyCommand"
)]
#[case(
    "ansible_ssh_extra_args",
    "-o ProxyCommand='ssh bastion",
    "ProxyCommand"
)]
#[case("ansible_connection", "carrier_pigeon", "carrier_pigeon")]
fn a_host_that_cannot_be_reached_is_refused_by_name(
    #[case] key: &str,
    #[case] value: &str,
    #[case] quoted: &str,
) {
    let err = Transport::for_host(&host(json!({ key: value })), &defaults()).unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains(key) || key == "ansible_connection", "{text}");
    assert!(text.contains(quoted), "{text}");
}
/// Two ssh identities of one name are two links.
#[test]
fn two_ports_of_one_host_are_two_keys() {
    use crate::executor::LinkKey;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let key = |port| LinkKey {
        host: "node".to_string(),
        become_user: None,
        become_program: None,
        transport: Transport::for_vars(
            "node",
            json!({"ansible_host": "10.0.0.5", "ansible_port": port})
                .as_object()
                .expect("an object"),
            &defaults(),
        )
        .expect("an ssh transport"),
    };
    let digest = |k: &LinkKey| {
        let mut h = DefaultHasher::new();
        k.hash(&mut h);
        h.finish()
    };

    assert_ne!(key(2222), key(2223));
    assert_ne!(digest(&key(2222)), digest(&key(2223)));
    assert_eq!(key(2222), key(2222));
}

const SHARED: Option<&str> = Some("/run/user/1000/volant-cm");

fn ssh_target(name: &str, vars: Value, dir: Option<&str>) -> SshTarget {
    match Transport::for_vars(name, vars.as_object().expect("an object"), &defaults())
        .map(|t| t.shared(dir.map(Path::new), name, None))
    {
        Ok(Transport::Ssh(target)) => target,
        other => panic!("expected an ssh transport, got {other:?}"),
    }
}

/// A shared connection carries its three options before the operator's words, and a keepalive of its own.
#[test]
fn a_shared_connection_carries_its_options_and_a_keepalive() {
    {
        let target = ssh_target(
            "web1",
            json!({"ansible_ssh_common_args": "-o ProxyJump=bastion"}),
            SHARED,
        );
        let path = target.control_path.clone().expect("a control path");
        assert!(path.starts_with("/run/user/1000/volant-cm"), "{path:?}");
        let argv = target.ssh_argv("true");
        let block = [
            "-o".to_string(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", path.display()),
            "-o".into(),
            "ControlPersist=30s".into(),
        ];
        let at = argv
            .windows(block.len())
            .position(|w| w == block)
            .unwrap_or_else(|| panic!("the three options, in order: {argv:?}"));
        let user = argv
            .iter()
            .position(|w| w == "ProxyJump=bastion")
            .expect("the operator's option");
        assert!(at < user, "{argv:?}");
    }
    {
        let argv = ssh_target("web1", json!({}), SHARED).ssh_argv("true");
        let block = [
            "-o".to_string(),
            "ServerAliveInterval=5".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
        ];
        assert!(argv.windows(block.len()).any(|w| w == block), "{argv:?}");
        for (vars, why) in [
            (json!({}), "not shared"),
            (
                json!({"ansible_ssh_common_args": "-o ControlPath=/x"}),
                "the operator's own sharing",
            ),
            (
                json!({"ansible_ssh_extra_args": "-o ServerAliveInterval=60"}),
                "the operator's own keepalive",
            ),
        ] {
            let dir = if why == "not shared" { None } else { SHARED };
            let text = ssh_target("web1", vars, dir).ssh_argv("true").join(" ");
            assert!(!text.contains("ServerAliveInterval=5"), "{why}: {text}");
        }
    }
}

/// Review focus: an operator who already set up connection sharing keeps theirs, whichever
/// variable carries it and however it is spelled.
#[test]
fn user_control_options_are_left_alone() {
    for (key, words) in [
        ("ansible_ssh_common_args", "-o ControlPath=/x"),
        ("ansible_ssh_extra_args", "-o ControlPath=/x"),
        ("ansible_ssh_extra_args", "-o ControlPersist=5m"),
        ("ansible_ssh_common_args", "-ocontrolmaster=no"),
        ("ansible_ssh_common_args", "-S /x"),
        ("ansible_ssh_extra_args", "-M"),
        ("ansible_ssh_extra_args", "-vvv"),
        ("ansible_ssh_common_args", "-4v"),
    ] {
        let target = ssh_target("web1", json!({ key: words }), SHARED);
        assert_eq!(target.control_path, None, "{key}={words}");
        let text = target.ssh_argv("true").join(" ");
        assert!(
            !text.contains("ControlMaster=auto") && !text.contains("volant-cm"),
            "{key}={words}: {text}"
        );
    }
    let plain = ssh_target("web1", json!({}), SHARED);
    assert!(plain.control_path.is_some(), "nothing set, so shared");
    for word in ["-lvolant", "-o", "LogLevel=DEBUG3", "-ivault.pem", "--"] {
        assert!(!asks_for_debug(word), "{word}");
    }
}

/// The same review focus, for the operator's ssh configuration file: `ssh -G` reads it the way
/// the connection will.
#[tokio::test]
async fn a_control_path_in_the_ssh_configuration_is_left_alone() {
    let guard = scratch();
    let dir = guard.path().to_path_buf();
    let file = dir.join("config");
    let target = ssh_target(
        "web1",
        json!({"ansible_ssh_common_args": format!("-F {}", file.display())}),
        SHARED,
    );
    assert!(target.control_path.is_some());
    std::fs::write(&file, "").expect("the configuration file");
    assert!(
        target.resolved().await.control_path.is_some(),
        "an empty configuration shares nothing; ssh -G must be able to run here"
    );
    for text in [
        "ControlPath /tmp/elsewhere-%C\n",
        "Host web1\n  ControlMaster auto\n",
        "ControlPersist 10m\n",
    ] {
        std::fs::write(&file, text).expect("the configuration file");
        assert_eq!(target.resolved().await.control_path, None, "{text:?}");
    }
}

/// What `ssh` resolves from its configuration goes into the socket name, so a changed
/// `HostName` never rides the old master.
#[tokio::test]
async fn a_host_moved_in_the_ssh_configuration_gets_a_new_socket() {
    let guard = scratch();
    let dir = guard.path().to_path_buf();
    let file = dir.join("config");
    let target = ssh_target(
        "web1",
        json!({"ansible_ssh_common_args": format!("-F {}", file.display())}),
        SHARED,
    );
    let mut paths = Vec::new();
    for text in [
        "Host web1\n  HostName 10.0.0.1\n",
        "Host web1\n  HostName 10.0.0.2\n",
        "Host web1\n  HostName 10.0.0.2\n  User ops\n",
        "Host web1\n  HostName 10.0.0.2\n  User ops\n  ProxyJump bastion\n",
        "Host web1\n  HostName 10.0.0.2\n  User ops\n  IdentityFile /k\n",
    ] {
        std::fs::write(&file, text).expect("the configuration file");
        let path = target.resolved().await.control_path.expect("shared");
        assert_eq!(path.parent(), Some(Path::new("/run/user/1000/volant-cm")));
        assert_eq!(
            path.as_os_str().len(),
            "/run/user/1000/volant-cm/".len() + 16
        );
        assert_eq!(
            target.resolved().await.control_path.as_deref(),
            Some(&*path),
            "one configuration, one socket: {text:?}"
        );
        paths.push(path);
    }
    let mut unique = paths.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), paths.len(), "{paths:?}");
}

/// Each (host, delegate) pair and each inventory name gets its own master.
#[test]
fn every_host_name_and_delegation_gets_its_own_master() {
    {
        let vars = json!({"ansible_host": "10.0.0.9"});
        let own = ssh_target("lb1", vars.clone(), SHARED).control_path;
        let mut paths: Vec<_> = (1..=20)
            .map(|n| {
                let transport =
                    Transport::for_vars("lb1", vars.as_object().expect("an object"), &defaults())
                        .expect("an ssh transport")
                        .shared(SHARED.map(Path::new), &format!("web{n}"), Some("lb1"));
                match transport {
                    Transport::Ssh(target) => target.control_path.expect("shared"),
                    Transport::Local => panic!("expected ssh"),
                }
            })
            .collect();
        assert!(paths.iter().all(|p| Some(p) != own.as_ref()));
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), 20, "one master per delegating host");

        // Two inventory aliases of one delegate, reached from one host: two masters, as the
        // aliases get when they are the hosts themselves.
        let via = |delegate: &str| match Transport::for_vars(
            delegate,
            vars.as_object().expect("an object"),
            &defaults(),
        )
        .expect("an ssh transport")
        .shared(SHARED.map(Path::new), "web1", Some(delegate))
        {
            Transport::Ssh(target) => target.control_path,
            Transport::Local => panic!("expected ssh"),
        };
        assert_ne!(via("lb1"), via("lb1-alias"));
    }
    {
        let vars = json!({"ansible_host": "10.0.0.5"});
        let one = ssh_target("web1", vars.clone(), SHARED);
        let two = ssh_target("web2", vars.clone(), SHARED);
        assert_eq!(one.address, two.address);
        assert_ne!(one.control_path, two.control_path);
        assert_eq!(
            one.control_path,
            ssh_target("web1", vars, SHARED).control_path,
            "one name, one path"
        );
        let other_user = ssh_target(
            "web1",
            json!({"ansible_host": "10.0.0.5", "ansible_user": "ops"}),
            SHARED,
        );
        assert_ne!(one.control_path, other_user.control_path);
    }
}

/// `ssh -O exit` reaches the socket the link would use, and returns even though nothing answers
/// there the way a master would.
#[cfg(unix)]
#[tokio::test]
async fn stopping_a_shared_connection_speaks_to_its_socket() {
    let guard = scratch();
    let dir = guard.path().to_path_buf();
    let mut target = ssh_target(
        "web1",
        json!({"ansible_ssh_common_args": "-F /dev/null"}),
        None,
    );
    target.control_path = Some(dir.join("0123456789abcdef").into_boxed_path());
    let socket = target.resolved().await.control_path.expect("shared");
    let listener = std::os::unix::net::UnixListener::bind(&*socket).expect("a socket");
    listener
        .set_nonblocking(true)
        .expect("a non-blocking socket");
    let transport = Transport::Ssh(target);
    let (_, accepted) = tokio::join!(
        tokio::time::timeout(STOP_MASTER_TIMEOUT * 2, transport.stop_shared()),
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // Accepted and dropped at once: `ssh -O` then reads end of file and gives up.
                match listener.accept() {
                    Ok(_) => return true,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(_) => return false,
                }
            }
        }),
    );
    assert_eq!(accepted, Ok(true), "ssh -O exit never came to {socket:?}");
}

/// `ssh` binds the socket under the path plus 17 bytes before renaming it, and macOS allows
/// 103: a longer path fails every connection of the host. Such a path is not used.
#[test]
fn a_control_path_too_long_to_bind_is_not_used() {
    let path = ssh_target("web1", json!({}), SHARED)
        .control_path
        .expect("a control path");
    assert!(path.as_os_str().len() <= CONTROL_PATH_MAX, "{path:?}");
    // A 69-byte directory makes an 86-byte path, the longest that binds everywhere.
    let longest = format!("/{}", "x".repeat(68));
    let path = ssh_target("web1", json!({}), Some(&longest)).control_path;
    assert_eq!(path.map(|p| p.as_os_str().len()), Some(CONTROL_PATH_MAX));
    let long = format!("/{}", "x".repeat(69));
    let target = ssh_target("web1", json!({}), Some(&long));
    assert_eq!(target.control_path, None);
    assert!(!target.ssh_argv("true").join(" ").contains("Control"));
}

/// Anybody able to put a socket in the directory can hand the next `ssh` a connection of their
/// own, so an existing directory must be this user's, at mode 0700.
#[cfg(unix)]
#[test]
fn the_socket_directory_is_refused_unless_it_is_private() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let guard = scratch();
    let tmp = guard.path().to_path_buf();
    // Whatever the umask made of it: a parent the group can write to is refused below.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let made = control_dir_in(None, &tmp, uid).expect("a fresh directory");
    assert_eq!(made, tmp.join(format!("volant-cm-{uid}")));
    let mode = std::fs::metadata(&made)
        .expect("created")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    assert_eq!(control_dir_in(None, &tmp, uid), Ok(made.clone()), "reused");

    std::fs::set_permissions(&made, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(
        control_dir_in(None, &tmp, uid).is_err(),
        "other permissions"
    );
    std::fs::set_permissions(&made, std::fs::Permissions::from_mode(0o700)).expect("chmod");

    // Created by this process, so owned by somebody other than the uid asked about.
    let other = uid + 1;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(tmp.join(format!("volant-cm-{other}")))
        .expect("a directory of the wrong owner");
    let err = control_dir_in(None, &tmp, other).expect_err("another owner");
    assert!(
        err.contains("owned by"),
        "the refusal names the wrong owner"
    );

    let runtime = tmp.join("runtime");
    std::fs::create_dir(&runtime).expect("a runtime directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o775)).expect("chmod");
    assert_eq!(
        control_dir_in(Some(&runtime), &tmp, uid),
        Ok(made.clone()),
        "a runtime directory the group can write to is passed over"
    );
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    assert_eq!(
        control_dir_in(Some(&runtime), &tmp, uid),
        Ok(runtime.join("volant-cm"))
    );
    let spaced = tmp.join("run time");
    std::fs::create_dir(&spaced).expect("a runtime directory with a space");
    assert_eq!(
        control_dir_in(Some(&spaced), &tmp, uid),
        Ok(made),
        "a path ssh would split is passed over"
    );

    let open = tmp.join("open");
    std::fs::create_dir(&open).expect("a shared parent");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    let err = control_dir_in(None, &open, uid).expect_err("a parent anybody can write to");
    assert!(
        err.contains("not sticky"),
        "the refusal names the missing sticky bit"
    );
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o1777)).expect("chmod");
    assert_eq!(
        control_dir_in(None, &open, uid),
        Ok(open.join(format!("volant-cm-{uid}"))),
        "a sticky parent, as /tmp is"
    );
}

#[cfg(unix)]
struct RestoreLimit(libc::rlimit);

#[cfg(unix)]
impl Drop for RestoreLimit {
    fn drop(&mut self) {
        // SAFETY: the struct is only read, and lives for the whole call.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) };
    }
}

#[cfg(unix)]
fn open_file_limit() -> libc::rlimit {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes into the struct it is given, which lives for the call.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    limit
}

/// The soft limit is lowered first: on a machine where it already equals the hard limit,
/// asserting on what is there would pass with the mechanism removed.
#[cfg(unix)]
#[test]
fn the_open_file_limit_is_raised_to_the_hard_limit() {
    let before = open_file_limit();
    let _restore = RestoreLimit(before);
    assert!(
        before.rlim_max > 256,
        "this test needs a hard limit above 256, found {}",
        before.rlim_max
    );
    let low = libc::rlimit {
        rlim_cur: 256,
        rlim_max: before.rlim_max,
    };
    // SAFETY: as above.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) }, 0);
    assert_eq!(open_file_limit().rlim_cur, 256);
    raise_open_file_limit();
    assert_eq!(open_file_limit().rlim_cur, before.rlim_max.min(1 << 20));
}
