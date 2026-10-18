// SPDX-License-Identifier: GPL-3.0-or-later
//! How a link escalates through `su`: the prompt, the password exchange and its failures.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::super::bootstrap::{Captured, EXIT_SSH_FAILURE, first_words};
use super::super::defaults::{ConnectError, Escalation, INCORRECT_SU_PASSWORD};
use super::super::shell::single_quoted;
use super::probe::{
    escalation_outcome, redacted_stderr, refused_authentication, unreachable_message,
};
use super::sudo::SudoForm;

/// The most `su` may print before its success line: a prompt and a refusal are a few dozen
/// bytes, and something printing without end is not `su` asking for anything.
const SU_TALK_LIMIT: usize = 4096;

/// `su`'s password prompts, the reference's `SU_PROMPT_LOCALIZATIONS` of ansible-core 2.19.12,
/// matched as su.py matches them: any of them, then optional spaces, then an ASCII or a
/// full-width colon, in any case.
fn su_prompt() -> &'static regex::Regex {
    static PROMPT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PROMPT.get_or_init(|| {
        const PROMPTS: [&str; 32] = [
            "Password",
            "암호",
            "パスワード",
            "Adgangskode",
            "Contraseña",
            "Contrasenya",
            "Hasło",
            "Heslo",
            "Jelszó",
            "Lösenord",
            "Mật khẩu",
            "Mot de passe",
            "Parola",
            "Parool",
            "Pasahitza",
            "Passord",
            "Passwort",
            "Salasana",
            "Sandi",
            "Senha",
            "Wachtwoord",
            "ססמה",
            "Лозинка",
            "Парола",
            "Пароль",
            "गुप्तशब्द",
            "शब्दकूट",
            "సంకేతపదము",
            "හස්පදය",
            "密码",
            "密碼",
            "口令",
        ];
        let alternatives = PROMPTS.map(regex::escape).join("|");
        regex::Regex::new(&format!(r"(?i)(?:{alternatives})\s*[:：]"))
            .expect("the prompt pattern is fixed")
    })
}

/// The remote command that runs `command` behind `su`, as the reference builds it: `become_exe`
/// or `su`, `become_flags` (none by default, so no `-` login), the user, then `-c` and the
/// command for `/bin/sh` whatever the user's own shell is. Every word is quoted for the remote
/// shell.
///
/// Before `command`, the shell `su` started makes its terminal raw: the agent's frames are
/// binary, and a terminal left as it is would echo them, turn their carriage returns into new
/// lines and read some of their bytes as signals. Only then does it print `success` (see
/// [`super::sudo::become_success`]), so nothing is sent before the terminal is ready for it.
pub(in crate::transport) fn su_command(
    escalation: &Escalation,
    command: &str,
    success: &str,
) -> String {
    let inner = format!("stty raw -echo -iexten || exit 1; echo {success}; {command}");
    let program = if escalation.exe.is_empty() {
        vec!["su".to_string()]
    } else {
        escalation.exe.clone()
    };
    let words = program
        .iter()
        .chain(escalation.flags.iter().flatten())
        .chain(std::iter::once(&escalation.user))
        .map(|word| single_quoted(word))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "exec {words} -c {}",
        single_quoted(&format!("/bin/sh -c {}", single_quoted(&inner)))
    )
}

/// Talks `su` through to the command it guards, the way the reference's ssh connection does:
/// a prompt is answered with the password once, and a second prompt or an `Authentication
/// failure` line is the wrong password. Without a password no prompt is answered, so `su` waits
/// and the link times out, which is what the reference reports too.
///
/// `success` is taken before any prompt as well, as the reference takes it: a connecting user
/// who is already root, or a PAM that lets this one in, gets no prompt at all. Being fresh for
/// every `su`, it can only come from the shell `su` started.
///
/// Reads one byte at a time, so nothing the command prints after `success` is taken off the
/// stream. `Ok(None)` is the command running; `Ok(Some(said))` is the stream ending before it
/// did, with what `su` had said, for the caller to tell a dead connection from a refusal by the
/// exit code.
pub(in crate::transport) async fn su_exchange<R, W>(
    out: &mut R,
    input: &mut W,
    password: Option<&str>,
    success: &str,
    timeout: Duration,
) -> Result<Option<String>, ConnectError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut said: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    let mut answered = false;
    let text = |bytes: &[u8]| redacted_stderr(&String::from_utf8_lossy(bytes), password);
    loop {
        let byte = match tokio::time::timeout(timeout, out.read_u8()).await {
            Ok(Ok(byte)) => byte,
            Ok(Err(_)) => {
                said.extend_from_slice(&line);
                return Ok(Some(text(&said)));
            }
            Err(_) => {
                // The reference's own words, with the complete lines `su` printed before it
                // went quiet; a prompt it is still waiting at has no line end and stays out.
                let msg = format!(
                    "Task failed: Timeout ({}s) waiting for privilege escalation prompt: {}",
                    timeout.as_secs(),
                    text(&said).trim()
                );
                return Err(ConnectError::Unreachable(msg.trim_end().to_string()));
            }
        };
        if said.len() + line.len() >= SU_TALK_LIMIT {
            return Err(ConnectError::Become(format!(
                "su printed more than {SU_TALK_LIMIT} bytes without starting the command: {}",
                first_words(&text(&said), "")
            )));
        }
        if byte == b'\n' {
            let complete = String::from_utf8_lossy(&line);
            // Ends with rather than equals: util-linux 2.41's `su` opens the session with an
            // `OSC 3008` context sequence that has no line end of its own, measured on the
            // development machine, and the reference too looks for the marker within the line.
            if complete.trim_end().ends_with(success) {
                return Ok(None);
            }
            if complete.contains("Authentication failure") {
                return Err(ConnectError::Become(INCORRECT_SU_PASSWORD.to_string()));
            }
            said.append(&mut line);
            said.push(b'\n');
            continue;
        }
        line.push(byte);
        // A prompt ends at its colon, the ASCII one or the last byte of the full-width one.
        let Some(password) = password.filter(|_| matches!(byte, b':' | 0x9A)) else {
            continue;
        };
        if !su_prompt().is_match(&String::from_utf8_lossy(&line)) {
            continue;
        }
        if answered {
            return Err(ConnectError::Become(INCORRECT_SU_PASSWORD.to_string()));
        }
        // A `su` that is gone by now closes the terminal, and the read above says so.
        let _ = input.write_all(format!("{password}\n").as_bytes()).await;
        let _ = input.flush().await;
        answered = true;
        line.clear();
    }
}

/// Why a command behind `su` never started, once the stream has ended: `ssh`'s own failure when
/// it exited 255, otherwise what `su` said, with the password taken back out.
pub(in crate::transport) fn su_ended(
    code: Option<i32>,
    said: &str,
    stderr: &str,
    password: Option<&str>,
) -> ConnectError {
    if code == Some(EXIT_SSH_FAILURE) {
        return ConnectError::Unreachable(unreachable_message(stderr, password));
    }
    ConnectError::Become(first_words(said, "su exited without starting the command"))
}

/// Which `sudo` form works for this link, asked of `sudo` itself rather than guessed from
/// whether a password happens to be available.
///
/// `-n` goes first, because a `sudo` that needs no authentication never reads stdin and a
/// password written to such a link would be read by the agent as its first frame header. Only
/// a refusal for want of authentication opens the password path, and only when there is a
/// password to offer: with none, `-n`'s refusal *is* the answer, and it is already worded for
/// it. Any other refusal is final, since no password can fix a missing `sudo`, a rule that
/// forbids the command, or a `sudo` that exited zero having run something else.
/// Returns the form and what the probe that settled it printed, so a caller whose probe is a
/// command it was going to run anyway does not have to run it twice.
pub(in crate::transport) async fn settle_form<F, Fut>(
    escalation: &Escalation,
    ran: fn(&Captured) -> bool,
    mut probe: F,
) -> Result<(SudoForm, Captured), ConnectError>
where
    F: FnMut(SudoForm) -> Fut,
    Fut: Future<Output = Result<Captured, ConnectError>>,
{
    let quiet = probe(SudoForm::NoPrompt).await?;
    let refusal = match escalation_outcome(ran(&quiet), &quiet.stderr, None) {
        Ok(()) => return Ok((SudoForm::NoPrompt, quiet)),
        Err(refusal) => refusal,
    };
    if escalation.password.is_none() || !refused_authentication(&quiet.stderr) {
        return Err(refusal);
    }
    let offered = probe(SudoForm::ReadStdin).await?;
    escalation_outcome(
        ran(&offered),
        &offered.stderr,
        escalation.password.as_deref(),
    )
    .map(|()| (SudoForm::ReadStdin, offered))
}
