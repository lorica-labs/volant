// SPDX-License-Identifier: GPL-3.0-or-later

use super::args::PlaybookArgs;
use std::path::Path;

/// A password file as ansible-core's `get_password_from_file` reads it: `-` is standard
/// input, an executable file is a script whose output is the password, anything else is read
/// and stripped of surrounding whitespace. The wording is the reference's.
pub(super) fn password_from_file(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let shown = path.display();
    let secret = if path == Path::new("-") {
        let mut bytes = Vec::new();
        std::io::stdin()
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Could not read password file '-': {e}"))?;
        bytes
    } else if !path.exists() {
        return Err(format!("The password file {shown} was not found"));
    } else if is_executable(path) {
        let out = std::process::Command::new(path)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| {
                format!(
                    "Problem occurred when trying to run the password script {shown} ({e}). If this is not a script, remove the executable bit from the file."
                )
            })?;
        if !out.status.success() {
            return Err(format!(
                "The password script {shown} returned an error (rc={}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        out.stdout
    } else {
        std::fs::read(path)
            .map_err(|_| format!("Could not read password file '{shown}'."))?
            .trim_ascii()
            .to_vec()
    };
    let end = secret
        .iter()
        .rposition(|b| !matches!(b, 13 | 10)) // CR, LF
        .map_or(0, |i| i + 1);
    let secret = &secret[..end];
    if secret.is_empty() {
        return Err(format!("Empty password was provided from file ({shown})"));
    }
    String::from_utf8(secret.to_vec())
        .map_err(|_| format!("The password file {shown} is not UTF-8"))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

/// The connection and the escalation password, asked in the reference's order. A prompt that
/// meets the end of the input ends the asking, and the run goes on with what it has, as
/// `ask_passwords` does when `getpass` raises `EOFError`. An empty escalation answer after `-k`
/// is the connection password.
pub(super) fn ask_passwords(
    args: &PlaybookArgs,
) -> Result<(Option<String>, Option<String>), String> {
    let ask = |prompt: &str| -> Result<Option<String>, String> {
        let Some(typed) = crate::prompt::hidden_line(prompt).map_err(|e| format!("{e:#}"))? else {
            return Ok(None);
        };
        // `from_utf8` rather than `from_utf8_lossy`: a password quietly rewritten with
        // replacement characters would be refused by the host, and the run would blame the
        // operator.
        String::from_utf8(typed)
            .map(Some)
            .map_err(|_| "the password is not UTF-8".to_string())
    };
    let mut connection = None;
    let mut become_prompt = "BECOME password: ";
    if args.ask_pass {
        connection = ask("SSH password: ")?;
        if connection.is_none() {
            return Ok((None, None));
        }
        become_prompt = "BECOME password[defaults to SSH password]: ";
    } else if let Some(path) = &args.connection_password_file {
        connection = Some(password_from_file(path)?);
    }
    let mut escalation = None;
    if args.ask_become_pass {
        escalation = ask(become_prompt)?;
        if args.ask_pass && escalation.as_deref() == Some("") {
            escalation.clone_from(&connection);
        }
    } else if let Some(path) = &args.become_password_file {
        escalation = Some(password_from_file(path)?);
    }
    Ok((connection, escalation))
}

/// The words of `--ssh-common-args` or `--ssh-extra-args`; an unbalanced quote is refused
/// by name instead of connecting without an option the operator asked for.
pub(super) fn ssh_words(flag: &str, text: Option<&str>) -> anyhow::Result<Vec<String>> {
    let Some(text) = text else {
        return Ok(Vec::new());
    };
    shlex::split(text).ok_or_else(|| {
        anyhow::anyhow!(
            "{flag} has unbalanced quotes and cannot be turned into ssh options: {text}"
        )
    })
}
