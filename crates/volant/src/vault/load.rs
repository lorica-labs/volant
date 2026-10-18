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
    Prompt,
    File(PathBuf),
}

/// `label@source`; a bare `source` has the label `default`. Only `prompt` is not a file.
pub(super) fn spec(text: &str) -> (String, Source) {
    let (label, source) = text.split_once('@').unwrap_or(("default", text));
    let source = if source == "prompt" {
        Source::Prompt
    } else {
        Source::File(PathBuf::from(source))
    };
    (label.to_string(), source)
}

/// `unfrackpath`: `~` expanded, made absolute, `.` and `..` folded away.
pub(super) fn absolute(path: &Path) -> PathBuf {
    let mut full = path.to_path_buf();
    if let (Ok(rest), Some(home)) = (path.strip_prefix("~"), std::env::var_os("HOME")) {
        full = Path::new(&home).join(rest);
    }
    let full = std::path::absolute(&full).unwrap_or(full);
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

fn prompt_secret(label: &str) -> Result<Vec<u8>, LoadError> {
    let text = if label == "default" {
        "Vault password: ".to_string()
    } else {
        format!("Vault password ({label}): ")
    };
    let fail = |message: String| LoadError {
        warning: format!("Error in vault password prompt ({label}): {message}"),
        message,
    };
    match crate::prompt::hidden_line(&text) {
        Ok(Some(typed)) => {
            typed_secret(&typed).ok_or_else(|| fail("Invalid vault password was provided".into()))
        }
        Ok(None) => Err(fail(format!("EOFError (ctrl-d) on prompt for ({label})"))),
        Err(e) => Err(fail(e.to_string())),
    }
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
    if request.ask {
        specs.push(("default".to_string(), Source::Prompt));
    }
    let mut secrets = Vec::new();
    for (label, source) in specs {
        secrets.push(match source {
            Source::Prompt => prompt_secret(&label)?,
            Source::File(path) => file_secret(&label, &path)?,
        });
    }
    Ok(Secrets(secrets))
}
