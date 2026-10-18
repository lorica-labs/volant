// SPDX-License-Identifier: GPL-3.0-or-later
//! The connection password, handed to `ssh` through `SSH_ASKPASS`. The run writes the password
//! to a private file; every `ssh` it starts is told to run this program as its askpass, which
//! prints the file's text. The password is never in an argument list, in the environment of
//! the program or in anything sent to the host: only the file's path is.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The environment variable that turns this program into an askpass for one `ssh` call.
const FILE_ENV: &str = "VOLANT_ASKPASS_FILE";

static FILE: OnceLock<PathBuf> = OnceLock::new();

/// Keeps the password file alive; the file is removed when this is dropped.
pub struct Installed(#[expect(dead_code, reason = "held for its Drop")] tempfile::NamedTempFile);

/// Stores the connection password in a file only this user can read, and makes every `ssh`
/// the run starts ask this program for it.
pub fn install(password: &str) -> std::io::Result<Installed> {
    use std::io::Write as _;
    let mut file = tempfile::Builder::new()
        .prefix("volant-askpass-")
        .tempfile()?;
    file.write_all(password.as_bytes())?;
    file.flush()?;
    FILE.set(file.path().to_path_buf())
        .map_err(|_| std::io::Error::other("a connection password is already installed"))?;
    Ok(Installed(file))
}

/// Whether the run has a connection password to offer.
pub fn installed() -> bool {
    FILE.get().is_some()
}

/// The environment an `ssh` of this run is started with, empty when there is no password.
pub fn environment() -> Vec<(&'static str, OsString)> {
    let (Some(file), Ok(me)) = (FILE.get(), std::env::current_exe()) else {
        return Vec::new();
    };
    vec![
        ("SSH_ASKPASS", me.into_os_string()),
        ("SSH_ASKPASS_REQUIRE", "force".into()),
        (FILE_ENV, file.clone().into_os_string()),
    ]
}

/// What `ssh` is told when it asks `prompt`: the password for a prompt that ends in
/// `password:`, nothing for any other question. A host-key confirmation or a key passphrase
/// may name a file or a host with "password" in it and must never get the password.
fn reply(file: &Path, prompt: &str) -> Option<Vec<u8>> {
    let lower = prompt.trim().to_ascii_lowercase();
    if lower.contains("passphrase") || lower.contains("yes/no") || !lower.ends_with("password:") {
        return None;
    }
    let mut text = std::fs::read(file).ok()?;
    text.push(b'\n');
    Some(text)
}

/// When `ssh` started this program as its askpass, answers and exits; otherwise returns.
pub fn answer_if_asked() {
    use std::io::Write as _;
    let Some(file) = std::env::var_os(FILE_ENV) else {
        return;
    };
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let code = match reply(Path::new(&file), &prompt) {
        Some(text) if std::io::stdout().write_all(&text).is_ok() => 0,
        _ => 1,
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests;
