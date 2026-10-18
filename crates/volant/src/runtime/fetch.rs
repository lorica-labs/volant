// SPDX-License-Identifier: GPL-3.0-or-later
//! Downloads checked against a pinned SHA-256 before anything uses them, and the Python archive
//! unpacked. The host's `curl` (or `wget`) and `tar` do the work; URLs and paths reach them as
//! arguments, never through a shell.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use sha2::{Digest, Sha256};

use super::INSTEAD;

/// Downloads `url` to `dest` when its SHA-256 is `sha256`.
///
/// The bytes land in a temporary file next to `dest` and are renamed into place only once their
/// hash matches, so `dest` never holds an unchecked or partial download. On a mismatch the
/// temporary file is deleted and the refusal gives both hashes.
pub(crate) fn fetch(url: &str, sha256: &str, dest: &Path) -> Result<(), String> {
    fetch_with(download, url, sha256, dest)
}

pub(super) fn fetch_with(
    download: impl FnOnce(&str, &Path) -> Result<(), String>,
    url: &str,
    sha256: &str,
    dest: &Path,
) -> Result<(), String> {
    let dir = dest
        .parent()
        .ok_or_else(|| format!("{} names no directory to download into", dest.display()))?;
    fs::create_dir_all(dir).map_err(|e| {
        format!(
            "cannot create {}: {e}; check the free disk space and the permissions",
            dir.display()
        )
    })?;
    let part = tempfile::Builder::new()
        .prefix(".download-")
        .tempfile_in(dir)
        .map_err(|e| {
            format!(
                "cannot create a file in {}: {e}; check the free disk space",
                dir.display()
            )
        })?;
    download(url, part.path())?;
    let actual = sha256_of(part.path())
        .map_err(|e| format!("cannot read back the download of {url}: {e}"))?;
    if actual != sha256 {
        return Err(format!(
            "the download of {url} does not match its pinned hash (expected sha256 {sha256}, \
             got {actual}), so it was deleted; try again, and report it if it happens again"
        ));
    }
    part.persist(dest).map_err(|e| {
        format!(
            "cannot move the download of {url} to {}: {}",
            dest.display(),
            e.error
        )
    })?;
    Ok(())
}

pub(crate) fn sha256_of(path: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Writes `url` to `out` with `curl`, or with `wget` when there is no `curl`.
fn download(url: &str, out: &Path) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err(format!("only https downloads are supported, not {url}"));
    }
    let mut curl = Command::new("curl");
    curl.args(["-fsSL", "--proto", "=https", "--proto-redir", "=https"])
        .args(["--retry", "2", "--connect-timeout", "30", "-o"])
        .arg(out)
        .arg(url);
    match run(curl) {
        Ok(output) => return judge("curl", url, &output),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot run curl: {e}")),
    }
    // Busybox's wget takes only these options, so it cannot keep redirects on https: the hash
    // check after the download is what vouches for the bytes.
    let mut wget = Command::new("wget");
    wget.args(["-q", "-T", "30", "-O"]).arg(out).arg(url);
    match run(wget) {
        Ok(output) => judge("wget", url, &output),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(format!(
            "neither curl nor wget is installed, and Volant needs one to download its Python: \
             install curl, or {INSTEAD}"
        )),
        Err(e) => Err(format!("cannot run wget: {e}")),
    }
}

fn run(mut command: Command) -> io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
}

/// The refusal for a downloader that failed, with the next step its exit code points to.
fn judge(program: &str, url: &str, output: &Output) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let next = match (program, output.status.code()) {
        ("curl", Some(5 | 6 | 7 | 28 | 35)) | ("wget", Some(4)) => {
            ": check the network connection and the proxy settings (https_proxy), then try again"
        }
        ("curl", Some(22 | 60)) | ("wget", Some(5 | 8)) => {
            ": check the URL, the proxy and the CA certificate settings, then try again"
        }
        ("curl", Some(23)) | ("wget", Some(3)) => ": check the free disk space, then try again",
        _ => "",
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "{program} could not download {url} ({}: {}){next}",
        output.status,
        stderr.trim()
    ))
}

/// Unpacks a python-build-standalone `install_only` archive into `dir` and returns the
/// interpreter it holds, `dir/python/bin/python3`. Only call it on an archive `fetch` checked.
/// `dir` must be absent or empty, so nothing left from an earlier run can pass for the archive's.
pub(crate) fn extract(tarball: &Path, dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut entries =
        fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    if entries.next().is_some() {
        return Err(format!(
            "{} is not empty, and Volant unpacks its Python only into an empty directory",
            dir.display()
        ));
    }
    let mut tar = Command::new("tar");
    tar.arg("-xzf").arg(tarball).arg("-C").arg(dir);
    let output = match run(tar) {
        Ok(output) => output,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "tar is not installed, and Volant needs it to unpack its Python: install tar, \
                 or {INSTEAD}"
            ));
        }
        Err(e) => return Err(format!("cannot run tar: {e}")),
    };
    if !output.status.success() {
        return Err(format!(
            "tar could not unpack {} into {} ({}: {}): check the free disk space, then try again",
            tarball.display(),
            dir.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let python = dir.join("python").join("bin").join("python3");
    if !python.is_file() {
        return Err(format!(
            "{} holds no python/bin/python3, so it is not a Python build Volant can use",
            tarball.display()
        ));
    }
    Ok(python)
}
