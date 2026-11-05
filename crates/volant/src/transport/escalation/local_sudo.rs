// SPDX-License-Identifier: GPL-3.0-or-later
//! `sudo` over a local connection, as the reference's local connection talks to it: the agent
//! starts behind a shell that first prints a fresh success line, a password goes out only when
//! `sudo` shows the fresh prompt it was given, and what either stream says before the success line
//! is never read as the agent's first frame.

use std::fmt::Write as _;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::process::Command;

use crate::agent::AgentLink;

use super::super::defaults::{ConnectError, Escalation};
use super::super::shell::single_quoted;
use super::probe::redacted_stderr;
use super::sudo::{become_success, interactive};

/// The reference's default `become_success_timeout`, also what a value under one second means.
const SUCCESS_TIMEOUT: Duration = Duration::from_secs(10);

/// Starts `agent` under `sudo` as `escalation` asks, with `soft` as its open file limit, and
/// returns the link over what follows the success line. `host` names the link in its `-vvv` lines.
pub(in crate::transport) async fn local_sudo(
    host: &str,
    agent: &str,
    escalation: &Escalation,
    soft: Option<u64>,
) -> Result<AgentLink, ConnectError> {
    let success = become_success();
    // As sudo.py asks `become_pass`: an empty password is none, and `sudo` is told not to ask.
    let password = escalation.password.as_deref().filter(|p| !p.is_empty());
    let prompt = password.map(|_| sudo_prompt(&success));
    let waiting = |reason: &str, said: &Said| said.message(reason, prompt.is_some(), password);
    let argv = sudo_argv(agent, escalation, prompt.as_deref(), soft, &success);
    let mut command = Command::new(&argv[0]);
    // `sudo_chdir`: `sudo` is started from there, as the reference's `cd` does.
    if let Some(dir) = escalation.chdir.as_deref() {
        command.current_dir(dir);
    }
    command.args(&argv[1..]);
    super::super::trace::local_link(host, &command);
    let spawned = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        // The reference's shell says so on stderr and ends, which ends the stream.
        Err(e) => {
            let said = Said {
                stderr: format!("{}: {e}\n", argv[0]).into_bytes(),
                ..Said::default()
            };
            return Err(ConnectError::Become(waiting(
                "Premature end of stream",
                &said,
            )));
        }
    };
    let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(ConnectError::Unreachable(
            "sudo was started without its pipes".to_string(),
        ));
    };
    let mut stdout = BufReader::new(stdout);
    let mut said = Said::default();
    let answer = prompt.as_deref().zip(password);
    let waited = tokio::time::timeout(
        success_timeout(escalation),
        until_success(
            (&mut stdout, &mut stderr, &mut stdin),
            answer,
            &success,
            &mut said,
        ),
    )
    .await;
    match waited {
        Ok(Waited::Success) => {}
        Ok(Waited::Ended) => {
            return Err(ConnectError::Become(waiting(
                "Premature end of stream",
                &said,
            )));
        }
        Ok(Waited::PromptedTwice) => {
            return Err(ConnectError::Become(waiting(
                "Duplicate become password prompt encountered",
                &said,
            )));
        }
        // The local connection's own words: the reference raises a connection failure for them.
        Err(_) => {
            let message = waiting("Timed out", &said);
            return Err(ConnectError::Unreachable(format!("Task failed: {message}")));
        }
    }
    // What stderr said just before the line may still be on its way.
    let mut chunk = [0; 4096];
    while let Ok(Ok(read @ 1..)) =
        tokio::time::timeout(Duration::from_millis(20), stderr.read(&mut chunk)).await
    {
        said.stderr.extend_from_slice(&chunk[..read]);
    }
    let preamble = said.preamble(escalation.keep_preamble, prompt.as_deref());
    tokio::spawn(async move {
        let mut chunk = [0; 4096];
        while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
            let _ = std::io::Write::write_all(&mut std::io::stderr(), &chunk[..read]);
        }
    });
    Ok(AgentLink::new_over(
        child,
        Box::new(stdin),
        Box::new(stdout),
        preamble,
    ))
}

/// `ansible_local_become_success_timeout`, or the default when it is under one second.
fn success_timeout(escalation: &Escalation) -> Duration {
    escalation
        .success_timeout
        .filter(|timeout| *timeout >= Duration::from_secs(1))
        .unwrap_or(SUCCESS_TIMEOUT)
}

/// sudo.py's prompt, keyed by what makes this `sudo`'s success line fresh.
fn sudo_prompt(success: &str) -> String {
    let key = success.rsplit('-').next().unwrap_or(success);
    format!("[sudo via ansible, key={key}] password:")
}

/// The command line sudo.py builds: `become_exe` or `sudo`, `become_flags` or `-H -S -n`, without
/// `-n` and with `-p` when a password can answer the prompt, then `-u` and a self-contained shell
/// command: a `become_exe` that is no `sudo` may run that command without its other words.
///
/// The shell checks the agent first, so an agent this user cannot run is a refusal in the shell's
/// words rather than a link that never answers. It sets the open file limit the run started with,
/// which a hook between fork and exec would make the standard library fork for on macOS.
fn sudo_argv(
    agent: &str,
    escalation: &Escalation,
    prompt: Option<&str>,
    soft: Option<u64>,
    success: &str,
) -> Vec<String> {
    let mut argv = if escalation.exe.is_empty() {
        vec!["sudo".to_string()]
    } else {
        escalation.exe.clone()
    };
    match (&escalation.flags, prompt) {
        (None, None) => argv.extend(["-H", "-S", "-n"].map(str::to_string)),
        (None, Some(_)) => argv.extend(["-H", "-S"].map(str::to_string)),
        (Some(flags), None) => argv.extend(flags.iter().cloned()),
        (Some(flags), Some(_)) => argv.extend(flags.iter().filter_map(|flag| interactive(flag))),
    }
    if let Some(prompt) = prompt {
        argv.extend(["-p".to_string(), prompt.to_string()]);
    }
    // An empty `become_user` names nobody, so `sudo` runs as its own default, root: sudo.py
    // leaves `-u` out for it.
    if !escalation.user.is_empty() {
        argv.extend(["-u".to_string(), escalation.user.clone()]);
    }
    let quoted = single_quoted(agent);
    let limit = soft.map_or(String::new(), |soft| {
        format!("ulimit -Sn {soft} 2>/dev/null; ")
    });
    let script = format!(
        "[ -x {quoted} ] || {{ echo {quoted}': this user cannot run the agent' >&2; exit 126; }}; \
         {limit}echo {success}; exec {quoted}"
    );
    argv.extend(["--", "/bin/sh", "-c"].map(str::to_string));
    argv.push(script);
    argv
}

/// How the wait for the success line ended, a timeout aside.
#[derive(Debug, PartialEq, Eq)]
enum Waited {
    Success,
    Ended,
    PromptedTwice,
}

/// What the two streams said so far, and how far each was read when the password went out.
#[derive(Default)]
struct Said {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    answered: Option<(usize, usize)>,
}

impl Said {
    /// The reference's words for a wait that ended without the success line, with what each stream
    /// said, the password taken back out.
    fn message(&self, reason: &str, prompted: bool, password: Option<&str>) -> String {
        let mut message = format!("{reason} waiting for become success");
        if prompted && self.answered.is_none() {
            message.push_str(" or become password prompt");
        }
        message.push('.');
        for (name, said) in [("Output", &self.stdout), ("Error", &self.stderr)] {
            if !said.is_empty() {
                let said = redacted_stderr(&String::from_utf8_lossy(said), password);
                let _ = write!(message, "\n>>> Standard {name}\n{said}");
            }
        }
        message.truncate(message.trim_end().len());
        message
    }

    /// What the link's `raw` tasks show. With `keep` that is all of it. Stripped, as the reference
    /// strips it, stdout has nothing left before the agent's frames, and stderr keeps only what
    /// follows the prompt, or all of it when the prompt was not said there.
    fn preamble(self, keep: bool, prompt: Option<&str>) -> Option<(Vec<u8>, Vec<u8>)> {
        if keep {
            return Some((self.stdout, self.stderr));
        }
        let kept = prompt.map_or(&self.stderr[..], |prompt| {
            after_first(&self.stderr, prompt.as_bytes())
        });
        (!kept.is_empty()).then(|| (Vec::new(), kept.to_vec()))
    }
}

/// `text` after the first `prompt` and the white space behind it, or all of it without one.
fn after_first<'a>(text: &'a [u8], prompt: &[u8]) -> &'a [u8] {
    match text.windows(prompt.len()).position(|at| at == prompt) {
        Some(start) => text[start + prompt.len()..].trim_ascii_start(),
        None => text,
    }
}

/// Whether one of the lines of `text` starts with `prompt`, as `check_password_prompt` asks.
fn prompted(text: &[u8], prompt: &str) -> bool {
    text.split(|byte| *byte == b'\n')
        .any(|line| line.trim_ascii().starts_with(prompt.as_bytes()))
}

/// Appends `chunk` to `kept`, up to and including a whole line that holds `success`: how much of
/// `chunk` that took, when it holds one. What follows it is the agent's.
fn through_success(kept: &mut Vec<u8>, chunk: &[u8], success: &[u8]) -> Option<usize> {
    let before = kept.len();
    kept.extend_from_slice(chunk);
    let mut start = kept[..before]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    while let Some(end) = kept[start..].iter().position(|byte| *byte == b'\n') {
        let end = start + end + 1;
        if kept[start..end]
            .windows(success.len())
            .any(|at| at == success)
        {
            kept.truncate(end);
            return Some(end - before);
        }
        start = end;
    }
    None
}

/// Reads both streams into `said` until a line of stdout holds `success`, which is kept, and
/// nothing behind it is taken off the stream. `answer` is the prompt and the password that answers
/// it: the first prompt on either stream gets it, once.
async fn until_success(
    (stdout, stderr, stdin): (
        &mut (impl AsyncBufRead + Unpin),
        &mut (impl AsyncRead + Unpin),
        &mut (impl AsyncWrite + Unpin),
    ),
    answer: Option<(&str, &str)>,
    success: &str,
    said: &mut Said,
) -> Waited {
    let mut chunk = [0; 4096];
    let (mut stdout_open, mut stderr_open) = (true, true);
    loop {
        if let Some((prompt, password)) = answer {
            let (out, err) = said.answered.unwrap_or_default();
            if prompted(&said.stdout[out..], prompt) || prompted(&said.stderr[err..], prompt) {
                if said.answered.is_some() {
                    return Waited::PromptedTwice;
                }
                said.answered = Some((said.stdout.len(), said.stderr.len()));
                // A `sudo` that is gone by now closes the streams, and the reads below say so.
                let _ = stdin.write_all(format!("{password}\n").as_bytes()).await;
                let _ = stdin.flush().await;
            }
        }
        if !stdout_open && !stderr_open {
            return Waited::Ended;
        }
        tokio::select! {
            read = stdout.fill_buf(), if stdout_open => {
                let read = read.unwrap_or_default();
                if read.is_empty() {
                    stdout_open = false;
                    continue;
                }
                let length = read.len();
                let through = through_success(&mut said.stdout, read, success.as_bytes());
                stdout.consume(through.unwrap_or(length));
                if through.is_some() {
                    return Waited::Success;
                }
            },
            read = stderr.read(&mut chunk), if stderr_open => match read {
                Ok(0) | Err(_) => stderr_open = false,
                Ok(read) => said.stderr.extend_from_slice(&chunk[..read]),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, duplex};

    use super::super::super::defaults::{BecomeMethod, Escalation};
    use super::{Said, Waited, sudo_argv, sudo_prompt, until_success};

    const SUCCESS: &str = "VOLANT-BECOME-SUCCESS-1";
    const PROMPT: &str = "[sudo via ansible, key=1] password:";

    fn escalation(flags: Option<&[&str]>) -> Escalation {
        Escalation {
            method: BecomeMethod::Sudo,
            user: "deploy".into(),
            password: None,
            exe: Vec::new(),
            flags: flags.map(|flags| flags.iter().map(ToString::to_string).collect()),
            prompt_l10n: Vec::new(),
            chdir: None,
            keep_preamble: false,
            success_timeout: None,
        }
    }

    #[test]
    fn the_command_line_asks_for_a_password_only_with_one_to_give() {
        let words = |flags, prompt| {
            let argv = sudo_argv("/a", &escalation(flags), prompt, None, SUCCESS);
            argv[..argv.len() - 1].join(" ")
        };
        assert_eq!(words(None, None), "sudo -H -S -n -u deploy -- /bin/sh -c");
        assert_eq!(
            words(None, Some(PROMPT)),
            format!("sudo -H -S -p {PROMPT} -u deploy -- /bin/sh -c")
        );
        assert_eq!(
            words(Some(&["-Hn", "-S", "-n"]), Some(PROMPT)),
            format!("sudo -H -S -p {PROMPT} -u deploy -- /bin/sh -c")
        );
        assert_eq!(sudo_prompt(SUCCESS), PROMPT);
        let argv = sudo_argv("/a b", &escalation(None), None, Some(64), SUCCESS);
        assert!(
            argv[argv.len() - 1].ends_with(&format!(
                "ulimit -Sn 64 2>/dev/null; echo {SUCCESS}; exec '/a b'"
            )),
            "{argv:?}"
        );
    }

    #[tokio::test]
    async fn a_prompt_is_answered_once_and_only_the_success_line_ends_the_wait() {
        // The prompt comes on stdout with no line end, and only then does anything go on.
        let (mut out_in, out) = duplex(4096);
        let (mut stdin, mut asked) = duplex(4096);
        let mut stdout = BufReader::new(out);
        let mut stderr: &[u8] = b"stderr noise\n";
        let sudo = tokio::spawn(async move {
            out_in.write_all(PROMPT.as_bytes()).await.unwrap();
            let mut answer = [0; 7];
            asked.read_exact(&mut answer).await.unwrap();
            assert_eq!(&answer, b"secret\n");
            out_in
                .write_all(format!("\n{SUCCESS}\n\0\0\0\x05frame").as_bytes())
                .await
                .unwrap();
            out_in
        });
        let mut said = Said::default();
        let waited = until_success(
            (&mut stdout, &mut stderr, &mut stdin),
            Some((PROMPT, "secret")),
            SUCCESS,
            &mut said,
        )
        .await;
        assert_eq!(waited, Waited::Success);
        let _open = sudo.await.unwrap();
        assert_eq!(said.stdout, format!("{PROMPT}\n{SUCCESS}\n").into_bytes());
        let mut frame = [0; 9];
        stdout.read_exact(&mut frame).await.unwrap();
        assert_eq!(
            &frame, b"\0\0\0\x05frame",
            "the agent's frames stay on the stream"
        );
    }

    #[tokio::test]
    async fn a_second_prompt_is_the_wrong_password_and_an_end_is_no_success() {
        // Each prompt comes once the one before it was answered, as `sudo` asks again.
        let (mut err_in, mut stderr) = duplex(4096);
        let (mut stdin, mut asked) = duplex(4096);
        let (_out_in, out) = duplex(4096);
        let mut stdout = BufReader::new(out);
        let sudo = tokio::spawn(async move {
            err_in.write_all(PROMPT.as_bytes()).await.unwrap();
            let mut answer = [0; 6];
            asked.read_exact(&mut answer).await.unwrap();
            let again = format!("\nSorry, try again.\n{PROMPT}");
            err_in.write_all(again.as_bytes()).await.unwrap();
            err_in
        });
        let mut said = Said::default();
        let waited = until_success(
            (&mut stdout, &mut stderr, &mut stdin),
            Some((PROMPT, "wrong")),
            SUCCESS,
            &mut said,
        )
        .await;
        let _open = sudo.await.unwrap();
        assert_eq!(waited, Waited::PromptedTwice);
        assert!(
            said.message(
                "Duplicate become password prompt encountered",
                true,
                Some("wrong")
            )
            .starts_with(
                "Duplicate become password prompt encountered waiting for become success.\n"
            )
        );

        let mut stdout: &[u8] = b"no success here\n";
        let mut stderr: &[u8] = b"err";
        let (mut stdin, _asked) = duplex(4096);
        let mut said = Said::default();
        let waited = until_success(
            (&mut stdout, &mut stderr, &mut stdin),
            None,
            SUCCESS,
            &mut said,
        )
        .await;
        assert_eq!(waited, Waited::Ended);
        assert_eq!(
            said.message("Premature end of stream", true, None),
            "Premature end of stream waiting for become success or become password prompt.\n\
             >>> Standard Output\nno success here\n\n>>> Standard Error\nerr"
        );
    }

    #[test]
    fn what_a_stripped_link_keeps_is_what_stderr_said_behind_the_prompt() {
        let said = || Said {
            stdout: format!("stdout noise\n{SUCCESS}\n").into_bytes(),
            stderr: format!("stderr noise\n{PROMPT}\nlater\n").into_bytes(),
            answered: None,
        };
        assert_eq!(
            said().preamble(true, Some(PROMPT)),
            Some((said().stdout, said().stderr))
        );
        assert_eq!(
            said().preamble(false, Some(PROMPT)),
            Some((Vec::new(), b"later\n".to_vec()))
        );
        let quiet = Said {
            stderr: format!("stderr noise\n{PROMPT}\n").into_bytes(),
            ..Said::default()
        };
        assert_eq!(quiet.preamble(false, Some(PROMPT)), None);
        let unprompted = Said {
            stderr: b"stderr noise\n".to_vec(),
            ..Said::default()
        };
        assert_eq!(
            unprompted.preamble(false, None),
            Some((Vec::new(), b"stderr noise\n".to_vec()))
        );
    }
}
