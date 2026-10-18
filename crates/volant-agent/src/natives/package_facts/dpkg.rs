// SPDX-License-Identifier: GPL-3.0-or-later
//! What dpkg's `status` holds: the installed packages, read as apt reads them.

use std::collections::HashMap;

/// The fields apt's `VersionHash` reads: a list's version and the status's are one record only
/// when these agree.
const HASHED: [&str; 6] = [
    "Installed-Size",
    "Depends",
    "Pre-Depends",
    "Conflicts",
    "Breaks",
    "Replaces",
];

/// One installed package as dpkg's status records it.
#[derive(Debug)]
pub(super) struct Installed<'a> {
    pub(super) version: &'a str,
    pub(super) arch: &'a str,
    pub(super) section: Option<&'a str>,
    pub(super) multi_arch: &'a str,
    pub(super) hashed: [Option<&'a str>; 6],
}

/// The paragraphs of a deb822 file.
pub(super) fn stanzas(text: &str) -> impl Iterator<Item = &str> {
    text.split("\n\n")
        .filter(|stanza| !stanza.trim().is_empty())
}

/// A field's value, continuation lines included, trimmed; names compare without case.
pub(super) fn field<'a>(stanza: &'a str, name: &str) -> Option<&'a str> {
    let mut lines = stanza.split('\n').peekable();
    while let Some(line) = lines.next() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if line.starts_with([' ', '\t']) || !key.eq_ignore_ascii_case(name) {
            continue;
        }
        let start = value.as_ptr() as usize - stanza.as_ptr() as usize;
        let mut end = start + value.len();
        while let Some(next) = lines.peek() {
            if !next.starts_with([' ', '\t']) {
                break;
            }
            end = next.as_ptr() as usize - stanza.as_ptr() as usize + next.len();
            lines.next();
        }
        return Some(stanza[start..end].trim());
    }
    None
}

/// `Multi-Arch` as apt reads it: absent is `no`; a value apt does not know hands back.
pub(super) fn multi_arch(stanza: &str) -> Result<&str, String> {
    match field(stanza, "Multi-Arch") {
        None => Ok("no"),
        Some(value @ ("no" | "same" | "foreign" | "allowed")) => Ok(value),
        Some(value) => Err(format!("Multi-Arch: {value} is not one apt knows")),
    }
}

pub(super) fn hashed(stanza: &str) -> [Option<&str>; 6] {
    HASHED.map(|name| field(stanza, name))
}

/// The native architecture (dpkg's own) and the packages whose `current_ver` apt sets.
///
/// apt sets it for every state but `not-installed` and `config-files`, so `half-configured`
/// or `unpacked` would be listed as installed: the native hands those back rather than
/// reading them.
pub(super) fn installed(status: &str) -> Result<(&str, HashMap<&str, Installed<'_>>), String> {
    let mut installed = HashMap::new();
    let mut native = None;
    for stanza in stanzas(status) {
        let name = field(stanza, "Package").ok_or("a status paragraph has no Package")?;
        let state = field(stanza, "Status").unwrap_or_default();
        let words: Vec<&str> = state.split_whitespace().collect();
        let [_want, flag, current] = words[..] else {
            return Err(format!("{name} has the status {state:?}"));
        };
        if matches!(current, "not-installed" | "config-files") {
            continue;
        }
        if flag != "ok" || current != "installed" {
            return Err(format!(
                "{name} is {flag} {current}, which the native does not read as installed"
            ));
        }
        let (Some(version), Some(arch)) = (field(stanza, "Version"), field(stanza, "Architecture"))
        else {
            return Err(format!(
                "{name} is installed without a version or an architecture"
            ));
        };
        if name == "dpkg" {
            native = Some(arch);
        }
        let package = Installed {
            version,
            arch,
            section: field(stanza, "Section"),
            multi_arch: multi_arch(stanza)?,
            hashed: hashed(stanza),
        };
        if installed.insert(name, package).is_some() {
            return Err(format!("{name} is installed twice"));
        }
    }
    let native = native.ok_or("dpkg itself is not installed")?;
    if let Some((name, package)) = installed
        .iter()
        .find(|(_, package)| package.arch != native && package.arch != "all")
    {
        return Err(format!(
            "{name} is installed for the foreign architecture {}",
            package.arch
        ));
    }
    Ok((native, installed))
}
