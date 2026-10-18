// SPDX-License-Identifier: GPL-3.0-or-later
//! A runtime packed into one file on a host that has a network, and installed from it on a
//! host that has none.

use std::fs;
use std::path::Path;
use std::process::Command;

use super::fetch::run;
use super::install::{self, Installed, Lock, RECORD, WAIT, lock_path, partial_prefix, probe};
use super::manifest::Manifest;
use super::{INSTEAD, host_triple};

/// Packs the installed runtime into `file`, a gzip tar holding one directory named like the
/// runtime. Written beside `file` and renamed, so a failed pack leaves nothing at `file`.
pub(crate) fn pack(runtimes: &Path, manifest: &Manifest, file: &Path) -> Result<(), String> {
    let id = manifest.id();
    if install::installed(runtimes, &id).is_none() {
        return Err(format!(
            "{id} is not installed: run `volant runtime install` first, then pack it"
        ));
    }
    let name = file
        .file_name()
        .ok_or_else(|| format!("{} is not a file name", file.display()))?;
    let mut partial = name.to_os_string();
    partial.push(".partial");
    let partial = file.with_file_name(partial);
    let mut tar = Command::new("tar");
    tar.arg("-czf")
        .arg(&partial)
        .arg("-C")
        .arg(runtimes)
        .arg(&id);
    let output = run(tar).map_err(|e| tar_error(&e, "pack"))?;
    if !output.status.success() {
        _ = fs::remove_file(&partial);
        return Err(format!(
            "tar could not pack {id} into {} ({}: {}): check the free disk space and the \
             directory's permissions",
            file.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    fs::rename(&partial, file).map_err(|e| {
        _ = fs::remove_file(&partial);
        format!("cannot write {}: {e}", file.display())
    })
}

fn tar_error(e: &std::io::Error, verb: &str) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!(
            "tar is not installed, and Volant needs it to {verb} a runtime: install tar, or {INSTEAD}"
        )
    } else {
        format!("cannot run tar: {e}")
    }
}

/// Checks a packed runtime's record against the host it is installed on.
fn check_record(record: &str, host: &str, ansible_core: &str) -> Result<(), String> {
    let record: serde_json::Value =
        serde_json::from_str(record).map_err(|e| format!("its {RECORD} is not valid JSON: {e}"))?;
    let field = |name: &str| record.get(name).and_then(|value| value.as_str());
    let packed = field("triple").ok_or_else(|| format!("its {RECORD} names no platform"))?;
    if packed != host {
        return Err(format!(
            "it was packed for {packed}, and this host needs {host}: pack it again on a host of \
             the same platform"
        ));
    }
    match field("ansible_core") {
        Some(version) if version == ansible_core => Ok(()),
        other => Err(format!(
            "it holds ansible-core {}, and this Volant pins {ansible_core}",
            other.unwrap_or("of no known version")
        )),
    }
}

/// Installs the runtime `manifest` pins from a file `pack` made, with no network. The file is
/// unpacked into a directory of its own, checked, and only then renamed into place. It is not
/// hash-checked as a download is: it is as trusted as the file you copied.
pub(crate) fn install_from(
    runtimes: &Path,
    manifest: &Manifest,
    file: &Path,
) -> Result<Installed, String> {
    let id = manifest.id();
    if let Some(done) = install::installed(runtimes, &id) {
        return Ok(done);
    }
    let host = host_triple()?;
    fs::create_dir_all(runtimes).map_err(|e| {
        format!(
            "cannot create {}: {e}; check the free disk space and the permissions",
            runtimes.display()
        )
    })?;
    let _lock = Lock::acquire(&lock_path(runtimes, &id), WAIT)?;
    if let Some(done) = install::installed(runtimes, &id) {
        return Ok(done);
    }
    install::sweep_partials(runtimes, &id);
    let work = tempfile::Builder::new()
        .prefix(&partial_prefix(&id))
        .tempdir_in(runtimes)
        .map_err(|e| {
            format!(
                "cannot create a directory in {}: {e}; check the free disk space",
                runtimes.display()
            )
        })?;
    let refuse = |why: String| {
        format!(
            "{} is not a runtime this host can install: {why}",
            file.display()
        )
    };
    let mut tar = Command::new("tar");
    tar.arg("-xzf").arg(file).arg("-C").arg(work.path());
    let output = run(tar).map_err(|e| tar_error(&e, "unpack"))?;
    if !output.status.success() {
        return Err(format!(
            "tar could not unpack {} ({}: {}): check that it is a file `volant runtime pack` \
             made and that the disk has room",
            file.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stage = work.path().join(&id);
    let record = fs::read_to_string(stage.join(RECORD)).map_err(|_| {
        let found: Vec<String> = fs::read_dir(work.path())
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect();
        refuse(format!(
            "it holds no {id} (it holds {}): pack it with the same Volant release",
            if found.is_empty() {
                "nothing".to_owned()
            } else {
                found.join(", ")
            }
        ))
    })?;
    check_record(&record, &host, &manifest.ansible_core).map_err(refuse)?;
    let python = stage.join("python").join("bin").join("python3");
    if !python.is_file() {
        return Err(refuse("it holds no python/bin/python3".to_owned()));
    }
    probe(&python, &manifest.ansible_core)?;
    let dir = runtimes.join(&id);
    if let Err(e) = fs::rename(&stage, &dir) {
        return install::installed(runtimes, &id).ok_or_else(|| {
            format!(
                "cannot move the runtime into {}: {e}; check the free disk space, then run the \
                 command again",
                dir.display()
            )
        });
    }
    install::installed(runtimes, &id)
        .ok_or_else(|| format!("{} is not usable after its install", dir.display()))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    #[rstest]
    #[case::same_host(r#"{"triple":"x86_64-unknown-linux-gnu","ansible_core":"2.19.12"}"#, Ok(()))]
    #[case::other_triple(
        r#"{"triple":"aarch64-unknown-linux-gnu","ansible_core":"2.19.12"}"#,
        Err(&["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"][..])
    )]
    #[case::other_libc(
        r#"{"triple":"x86_64-unknown-linux-musl","ansible_core":"2.19.12"}"#,
        Err(&["x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"][..])
    )]
    #[case::other_ansible(
        r#"{"triple":"x86_64-unknown-linux-gnu","ansible_core":"2.18.1"}"#,
        Err(&["2.18.1", "2.19.12"][..])
    )]
    #[case::no_triple(r#"{"ansible_core":"2.19.12"}"#, Err(&["names no platform"][..]))]
    #[case::not_json("nope", Err(&["not valid JSON"][..]))]
    fn a_pack_is_checked_against_the_host(
        #[case] record: &str,
        #[case] expected: Result<(), &[&str]>,
    ) {
        let got = super::check_record(record, "x86_64-unknown-linux-gnu", "2.19.12");
        match (got, expected) {
            (Ok(()), Ok(())) => {}
            (Err(e), Err(words)) => assert!(words.iter().all(|word| e.contains(word)), "{e}"),
            (got, expected) => panic!("got {got:?}, expected {expected:?}"),
        }
    }
}
