// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading the secrets a run is given: files, scripts and prompts.

use std::path::{Component, Path, PathBuf};

use super::Secrets;
use super::format::strip_crlf;

/// Why a secret could not be loaded: the warning line the reference prints first, and the error.
#[derive(Debug, PartialEq, Eq)]
pub struct LoadError {
    pub warning: String,
    pub message: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Source {
    /// `prompt`: asks as `Vault password (label): `.
    Prompt,
    /// `prompt_ask_vault_pass`, and what `--ask-vault-pass` and a command with no secret at all
    /// ask: the older `Vault password: ` wording.
    AskPass,
    File(PathBuf),
}

/// `label@source`; a bare `source` has the label `default`, and so does an empty label. Only
/// `prompt` and `prompt_ask_vault_pass` are not files.
pub(super) fn spec(text: &str) -> (String, Source) {
    let (label, source) = text.split_once('@').unwrap_or(("default", text));
    let label = if label.is_empty() { "default" } else { label };
    let source = match source {
        "prompt" => Source::Prompt,
        "prompt_ask_vault_pass" => Source::AskPass,
        _ => Source::File(PathBuf::from(source)),
    };
    (label.to_string(), source)
}

/// `unfrackpath`: `~` expanded, made absolute, `.` and `..` folded away.
pub fn absolute(path: &Path) -> PathBuf {
    let mut full = path.to_path_buf();
    if let (Ok(rest), Some(home)) = (path.strip_prefix("~"), std::env::var_os("HOME")) {
        full = Path::new(&home).join(rest);
    }
    normalize(&full)
}

/// `os.path.abspath`: made absolute, `.` and `..` folded away, no `~`, no links followed.
pub fn normalize(path: &Path) -> PathBuf {
    let full = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for part in full.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The reference's `is_executable`: any execute bit in the mode, whoever the caller is.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Controllers run on Unix; elsewhere every password file is read as text.
#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

/// The exit status as the reference prints it: the code, or minus the signal.
fn exit_number(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

fn file_secret(label: &str, path: &Path) -> Result<Vec<u8>, LoadError> {
    let path = absolute(path);
    let getting = |message: String| LoadError {
        warning: format!("Error getting vault password file ({label}): {message}"),
        message,
    };
    let loading = |message: String| LoadError {
        warning: format!("Error in vault password file loading ({label}): {message}"),
        message,
    };
    if !path.exists() {
        return Err(getting(format!(
            "The vault password file {} was not found",
            path.display()
        )));
    }
    if path.is_dir() {
        return Err(getting(format!(
            "The vault password file provided '{}' can not be a directory",
            path.display()
        )));
    }
    let (secret, kind) = if is_executable(&path) {
        let out = std::process::Command::new(&path)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .output()
            .map_err(|e| {
                loading(format!(
                    "Problem running vault password script {} ({e}). If this is not a script, remove the executable bit from the file.",
                    path.display()
                ))
            })?;
        if !out.status.success() {
            let rc = exit_number(out.status);
            return Err(loading(format!(
                "Vault password script {} returned non-zero ({rc}): None",
                path.display()
            )));
        }
        (strip_crlf(&out.stdout).to_vec(), "script")
    } else {
        let bytes = std::fs::read(&path).map_err(|_| {
            loading(format!(
                "Could not read vault password file '{}'.",
                path.display()
            ))
        })?;
        (bytes.trim_ascii().to_vec(), "file")
    };
    if secret.is_empty() {
        return Err(loading(format!(
            "Invalid vault password was provided from {kind} ({})",
            path.display()
        )));
    }
    Ok(secret)
}

/// What was typed, surrounding whitespace removed; `None` when nothing is left.
pub(super) fn typed_secret(typed: &[u8]) -> Option<Vec<u8>> {
    Some(typed.trim_ascii().to_vec()).filter(|s| !s.is_empty())
}

/// One secret from the terminal. A new secret is asked twice and the two must match, as the
/// reference does for `encrypt`.
fn prompt_secret(label: &str, ask_pass: bool, new: bool) -> Result<Vec<u8>, LoadError> {
    let (first, again) = match (ask_pass, new) {
        (true, false) => ("Vault password: ".to_string(), None),
        (true, true) => (
            "New Vault password: ".to_string(),
            Some("Confirm New Vault password: ".to_string()),
        ),
        (false, false) => (format!("Vault password ({label}): "), None),
        (false, true) => (
            format!("New vault password ({label}): "),
            Some(format!("Confirm new vault password ({label}): ")),
        ),
    };
    let fail = |message: String| LoadError {
        warning: format!("Error in vault password prompt ({label}): {message}"),
        message,
    };
    let ask = |text: &str| match crate::prompt::hidden_line(text) {
        Ok(Some(typed)) => {
            typed_secret(&typed).ok_or_else(|| fail("Invalid vault password was provided".into()))
        }
        Ok(None) => Err(fail(format!("EOFError (ctrl-d) on prompt for ({label})"))),
        Err(e) => Err(fail(e.to_string())),
    };
    let secret = ask(&first)?;
    if let Some(again) = again
        && ask(&again)? != secret
    {
        return Err(fail("Passwords do not match".into()));
    }
    Ok(secret)
}

/// What the command line and the configuration ask for, in the reference's order: identity list
/// from the configuration, `--vault-id` entries, then the password files, then the prompt.
#[derive(Debug, Default)]
pub struct Request {
    pub ask: bool,
    pub password_files: Vec<PathBuf>,
    pub ids: Vec<String>,
    pub config_file: Option<PathBuf>,
    pub config_ids: Vec<String>,
}

pub fn load(request: &Request) -> Result<Secrets, LoadError> {
    let labelled = gather(request, false, false)?;
    Ok(Secrets(
        labelled.into_iter().map(|(_, secret)| secret).collect(),
    ))
}

/// The secrets with the label each one was given, for the `vault` command. With no source at
/// all it asks on the terminal, and `new` asks twice, for a secret that is about to encrypt.
pub fn load_labelled(request: &Request, new: bool) -> Result<Vec<(String, Vec<u8>)>, LoadError> {
    gather(request, new, true)
}

fn gather(
    request: &Request,
    new: bool,
    prompt_when_empty: bool,
) -> Result<Vec<(String, Vec<u8>)>, LoadError> {
    let mut specs: Vec<(String, Source)> = request
        .config_ids
        .iter()
        .chain(&request.ids)
        .map(|s| spec(s))
        .collect();
    specs.extend(
        request
            .password_files
            .iter()
            .chain(&request.config_file)
            .map(|p| ("default".to_string(), Source::File(p.clone()))),
    );
    if request.ask || (prompt_when_empty && specs.is_empty()) {
        specs.push(("default".to_string(), Source::AskPass));
    }
    let mut secrets = Vec::new();
    for (label, source) in specs {
        let secret = match source {
            Source::Prompt => prompt_secret(&label, false, new)?,
            Source::AskPass => prompt_secret(&label, true, new)?,
            Source::File(path) => file_secret(&label, &path)?,
        };
        secrets.push((label, secret));
    }
    Ok(secrets)
}
