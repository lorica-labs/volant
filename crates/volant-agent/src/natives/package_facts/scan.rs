// SPDX-License-Identifier: GPL-3.0-or-later
//! The lists' versions of the installed names, and the record apt makes from them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use super::dpkg::{Installed, field, hashed, multi_arch, stanzas};
use crate::modules::command::CANCEL_POLL;
use crate::natives::runtime::{Clock, Stop};

/// One version of an installed package's name found in a list.
#[derive(Debug)]
pub(super) struct Listed {
    origin: String,
    version: String,
    arch: String,
    section: Option<String>,
    size: Option<String>,
    multi_arch: String,
    hashed: [Option<String>; 6],
}

pub(super) type ScanResult = Result<Vec<(String, Listed)>, Stop>;

/// Every list's versions of the installed names, read in parallel. The cancel is only asked
/// from this thread; on it, or at the deadline, the readers are told to stop.
pub(super) fn scan_all(
    files: &[(PathBuf, String)],
    installed: &HashMap<&str, Installed<'_>>,
    clock: Clock,
    scan: impl Fn(&PathBuf, &str, &HashMap<&str, Installed<'_>>, &AtomicBool) -> ScanResult + Sync,
) -> Result<HashMap<String, Vec<Listed>>, Stop> {
    let scan = &scan;
    let halt = AtomicBool::new(false);
    let (sent, answer) = mpsc::channel();
    std::thread::scope(|scope| {
        for (path, origin) in files {
            let sent = sent.clone();
            let halt = &halt;
            scope.spawn(move || {
                let _ = sent.send(scan(path, origin, installed, halt));
            });
        }
        drop(sent);
        let mut found: HashMap<String, Vec<Listed>> = HashMap::new();
        let mut left = files.len();
        while left > 0 {
            match answer.recv_timeout(CANCEL_POLL) {
                Ok(listed) => {
                    left -= 1;
                    for (name, listed) in listed? {
                        found.entry(name).or_default().push(listed);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if (clock.cancelled)() {
                        halt.store(true, Ordering::Relaxed);
                        return Err(Stop::Cancelled);
                    }
                    if clock
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        halt.store(true, Ordering::Relaxed);
                        return Err(Stop::TimedOut);
                    }
                }
            }
        }
        Ok(found)
    })
}

/// One list's versions of the installed names. Stops early, with what it has, when `halt`
/// is raised: the caller has already given up on the answer.
pub(super) fn scan(
    path: &PathBuf,
    origin: &str,
    installed: &HashMap<&str, Installed<'_>>,
    halt: &AtomicBool,
) -> ScanResult {
    let bytes =
        std::fs::read(path).map_err(|err| format!("{} cannot be read: {err}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
    let mut listed = Vec::new();
    for (count, stanza) in stanzas(&text).enumerate() {
        if count % 1024 == 0 && halt.load(Ordering::Relaxed) {
            break;
        }
        // Every list paragraph starts with its `Package`; reading only that line skips the
        // thousands of packages this host does not have.
        let name = match stanza.trim_start_matches('\n').strip_prefix("Package: ") {
            Some(rest) => rest.split('\n').next().unwrap_or_default().trim(),
            None => field(stanza, "Package").unwrap_or_default(),
        };
        if !installed.contains_key(name) {
            continue;
        }
        let (Some(version), Some(arch)) = (field(stanza, "Version"), field(stanza, "Architecture"))
        else {
            return Err(format!("{name} is listed without a version or an architecture").into());
        };
        listed.push((
            name.to_string(),
            Listed {
                origin: origin.to_string(),
                version: version.to_string(),
                arch: arch.to_string(),
                section: field(stanza, "Section").map(str::to_string),
                size: field(stanza, "Size").map(str::to_string),
                multi_arch: multi_arch(stanza)?.to_string(),
                hashed: hashed(stanza).map(|value| value.map(str::to_string)),
            },
        ));
    }
    Ok(listed)
}

/// The section and origin of the record apt makes the package's current version: the lists'
/// when a list carries the installed version with the fields apt hashes unchanged, the
/// status's own (and the empty origin) when none does.
pub(super) fn record(
    name: &str,
    package: &Installed<'_>,
    listed: &[Listed],
) -> Result<(String, String), String> {
    let mut same = Vec::new();
    for list in listed.iter().filter(|list| list.arch == package.arch) {
        if list.version != package.version {
            if canonical(&list.version) == canonical(package.version) {
                return Err(format!(
                    "{name} is listed as {} and installed as {}, which apt compares equal",
                    list.version, package.version
                ));
            }
            continue;
        }
        let hashed_same = list
            .hashed
            .iter()
            .zip(package.hashed)
            .all(|(list, status)| list.as_deref() == status);
        if !hashed_same || list.multi_arch != package.multi_arch {
            return Err(format!(
                "{name} {} is listed with fields apt hashes that differ from the status",
                package.version
            ));
        }
        same.push(list);
    }
    let Some(first) = same.first() else {
        let section = package
            .section
            .ok_or_else(|| format!("{name} has no section"))?;
        return Ok((section.to_string(), String::new()));
    };
    // apt keeps the first list's record, with the lists in the order of the sources; when
    // they all agree, that order does not matter.
    if same.iter().any(|list| {
        list.origin != first.origin || list.section != first.section || list.size != first.size
    }) {
        return Err(format!(
            "{name} {} is listed in sources that disagree about it",
            package.version
        ));
    }
    let section = first
        .section
        .clone()
        .ok_or_else(|| format!("{name} is listed without a section"))?;
    Ok((section, first.origin.clone()))
}

/// A version as dpkg compares it: epoch, upstream and revision with the leading zeros of each
/// number dropped and an absent part read as zero. Two versions are equal for apt exactly
/// when these are.
pub(super) fn canonical(version: &str) -> String {
    let (epoch, rest) = version.split_once(':').unwrap_or(("", version));
    let (upstream, revision) = rest.rsplit_once('-').unwrap_or((rest, ""));
    let numbers = |part: &str| {
        let mut out = String::new();
        let mut digits = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() {
                digits.push(c);
            } else {
                out.push_str(digits.trim_start_matches('0'));
                digits.clear();
                out.push(c);
            }
        }
        out.push_str(digits.trim_start_matches('0'));
        out
    };
    format!(
        "{}:{}-{}",
        numbers(epoch),
        numbers(upstream),
        numbers(revision)
    )
}
