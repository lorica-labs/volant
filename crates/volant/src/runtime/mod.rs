// SPDX-License-Identifier: GPL-3.0-or-later
//! The controller's own Python and ansible-core: the pinned builds of `manifest.toml`, the host
//! platform they are chosen for, their download, checked against the pinned hash before use, and
//! the runtimes installed from them.

pub(crate) mod collections;
pub(crate) mod fetch;
pub(crate) mod install;
pub(crate) mod manifest;
pub(crate) mod resolve;

pub use resolve::Mode;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What a refusal to manage the runtime tells the user to do instead.
pub(crate) const INSTEAD: &str =
    "set VOLANT_PYTHON to an interpreter that has ansible-core 2.19.12";

/// The directory the runtimes are installed in: `<data>/volant/runtime`, where `<data>` is
/// `$XDG_DATA_HOME` or `~/.local/share`, and `~/Library/Application Support` on macOS. Relative
/// values are ignored, as for the cache.
pub(crate) fn runtimes_dir() -> Result<PathBuf, String> {
    let absolute = |var: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let data = if cfg!(target_os = "macos") {
        absolute("HOME").map(|home| home.join("Library").join("Application Support"))
    } else {
        absolute("XDG_DATA_HOME").or_else(|| absolute("HOME").map(|home| home.join(".local/share")))
    };
    data.map(|data| data.join("volant").join("runtime"))
        .ok_or_else(|| {
            format!(
                "neither XDG_DATA_HOME nor HOME names an absolute directory to install the \
                 Python runtime in: set HOME, or {INSTEAD}"
            )
        })
}

/// What `volant runtime status` says about the managed runtime: its name, whether and where
/// it is installed, the platform it is for, and the other runtimes left installed.
pub(crate) fn status(runtimes: &Path, manifest: &manifest::Manifest) -> String {
    let id = manifest.id();
    let mut lines = resolve::describe();
    lines.push(format!("managed runtime: {id}"));
    lines.push(match install::installed(runtimes, &id) {
        Some(done) => format!("  installed: {}", done.python.display()),
        None => format!(
            "  not installed: `volant runtime install` puts it in {}",
            runtimes.join(&id).display()
        ),
    });
    lines.push(match host_triple() {
        Ok(triple) => format!("  platform: {triple}"),
        Err(e) => format!("  platform: {e}"),
    });
    let others: Vec<String> = install::list(runtimes)
        .into_iter()
        .filter(|other| *other != id)
        .collect();
    if !others.is_empty() {
        lines.push(format!(
            "other runtimes (`volant runtime remove <name>` deletes one): {}",
            others.join(", ")
        ));
    }
    lines.join("\n") + "\n"
}

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
