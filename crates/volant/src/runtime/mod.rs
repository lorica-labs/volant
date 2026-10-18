// SPDX-License-Identifier: GPL-3.0-or-later
//! The controller's own Python and ansible-core: the pinned builds of `manifest.toml`, the host
//! platform they are chosen for, and their download, checked against the pinned hash before use.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the commands that install the runtime come next")
)]

pub(crate) mod fetch;
pub(crate) mod manifest;

use std::path::Path;
use std::process::{Command, Stdio};

/// What a refusal to manage the runtime tells the user to do instead.
pub(crate) const INSTEAD: &str =
    "set VOLANT_PYTHON to an interpreter that has ansible-core 2.19.12";

/// The manifest triple whose Python runs on this host: its architecture and, on Linux, its C
/// library. Volant's own build target does not decide it: a static musl controller on a glibc
/// host needs the glibc Python.
pub(crate) fn host_triple() -> Result<String, String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let musl = || {
        let ldd = Command::new("ldd")
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .ok()
            .map(|out| {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                text
            });
        is_musl(
            ldd.as_deref(),
            Path::new(&format!("/lib/ld-musl-{arch}.so.1")).exists(),
        )
    };
    triple(os, arch, musl)
}

/// The triple for `os` and `arch`; `musl` is only asked on Linux.
fn triple(os: &str, arch: &str, musl: impl FnOnce() -> bool) -> Result<String, String> {
    match (os, arch) {
        ("linux", "x86_64" | "aarch64") => {
            let libc = if musl() { "musl" } else { "gnu" };
            Ok(format!("{arch}-unknown-linux-{libc}"))
        }
        ("macos", "x86_64" | "aarch64") => Ok(format!("{arch}-apple-darwin")),
        _ => Err(format!(
            "a managed Python is not supported on {os} {arch} yet: {INSTEAD}"
        )),
    }
}

/// Whether the host's C library is musl, from what `ldd --version` printed (glibc's names
/// itself, musl's names itself) or, when that says neither, from musl's loader being installed.
fn is_musl(ldd: Option<&str>, musl_loader: bool) -> bool {
    let ldd = ldd.unwrap_or_default().to_ascii_lowercase();
    if ldd.contains("musl") {
        true
    } else if ldd.contains("glibc") || ldd.contains("gnu libc") {
        false
    } else {
        musl_loader
    }
}

#[cfg(test)]
mod tests;
