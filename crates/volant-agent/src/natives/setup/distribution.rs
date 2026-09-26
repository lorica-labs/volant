// SPDX-License-Identifier: GPL-3.0-or-later
//! The `distribution` collector on Debian and Ubuntu, and the gate of the whole native: any other
//! `ID` hands back.
//!
//! The reference takes a first guess from the `distro` library (`os-release`, `lsb_release -a`,
//! `/etc/debian_version`), then walks its list of release files, where Debian and Ubuntu are
//! recognised by `/etc/os-release` once every earlier entry has missed. The native follows both,
//! and hands back wherever one of them would read something it does not: an earlier release file,
//! a `distro` release file, a version it cannot rank without the library's regular expressions.

use serde_json::{Map, Value};

use super::{Host, LsbRelease, Root, py_strip, splitlines};

/// Release files the reference reads before `/etc/os-release` as Debian's, each of which names
/// another distribution when it exists.
const EARLIER_FILES: &[&str] = &[
    "/etc/altlinux-release",
    "/etc/oracle-release",
    "/etc/slackware-version",
    "/etc/centos-release",
    "/etc/redhat-release",
    "/etc/openwrt_release",
    "/etc/system-release",
    "/etc/alpine-release",
    "/etc/SuSE-release",
    "/etc/gentoo-release",
];

/// The same, taken even when empty.
const EARLIER_FILES_EVEN_EMPTY: &[&str] = &["/etc/vmware-release", "/etc/arch-release"];

/// The names `distro` does not take for a release file.
const DISTRO_IGNORED: &[&str] = &[
    "debian_version",
    "lsb-release",
    "oem-release",
    "os-release",
    "system-release",
    "plesk-release",
    "iredmail-release",
    "board-release",
    "ec2_version",
];

/// `/etc/os-release` as `distro` reads it, and the distribution's normalised `ID`, which must be
/// `ubuntu` or `debian`: the check the native makes before it spends anything on a host.
pub fn gate(root: &Root) -> Result<(Vec<(String, String)>, String), String> {
    if !root.is_file("/etc/os-release") {
        return Err("/etc/os-release is missing".into());
    }
    let os_release = std::fs::read(root.path("/etc/os-release"))
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or("/etc/os-release is unreadable or not UTF-8")?;
    let os = parse_os_release(&os_release)?;
    let id = last(&os, "id")
        .unwrap_or_default()
        .to_lowercase()
        .replace(' ', "_");
    if id != "ubuntu" && id != "debian" {
        return Err(format!(
            "the distribution is '{id}', outside Debian and Ubuntu"
        ));
    }
    Ok((os, id))
}

/// A key given twice keeps its last value, as in the library's dictionary.
fn last<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

pub fn collect(host: &Host, lsb_release: &LsbRelease) -> Result<Map<String, Value>, String> {
    let root = host.root;
    for variable in ["UNIXCONFDIR", "UNIXUSRLIBDIR"] {
        if host.env.contains_key(variable) {
            return Err(format!("{variable} moves where distro reads"));
        }
    }
    // `ansible.module_utils.distro` imports a system `distro` when there is one. Before 1.8 it
    // did not rank `/etc/debian_version` among the versions, and the native follows 1.9.
    match host.probe.distro.as_deref() {
        None => {}
        Some(version) if version.starts_with("1.9.") => {}
        Some(version) => {
            return Err(format!(
                "the interpreter imports distro '{version}', not the bundled 1.9"
            ));
        }
    }
    let (os, id) = gate(root)?;
    let get = |key: &str| last(&os, key);
    for path in EARLIER_FILES {
        if std::fs::metadata(root.path(path)).is_ok_and(|meta| meta.is_file() && meta.len() > 0) {
            return Err(format!("{path} names another distribution"));
        }
    }
    for path in EARLIER_FILES_EVEN_EMPTY {
        if root.is_file(path) {
            return Err(format!("{path} names another distribution"));
        }
    }
    // What the reference parses: the file stripped, then its quotes and backslashes stripped.
    let data = root
        .content("/etc/os-release")
        .ok_or("/etc/os-release is empty")?;
    let data = data.trim_matches(['\'', '"', '\\']);
    if data.contains("Amazon")
        || data.contains("Arch Linux")
        || data.to_lowercase().contains("suse")
    {
        return Err("/etc/os-release reads as another distribution to the reference".into());
    }
    let etc = std::fs::read_dir(root.path("/etc")).map_err(|err| format!("listing /etc: {err}"))?;
    for entry in etc.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if release_file_name(&name)
            && !DISTRO_IGNORED.contains(&name.as_str())
            && entry.path().is_file()
        {
            return Err(format!("/etc/{name} is a release file distro would read"));
        }
    }
    let debian_version = debian_version(host)?;
    let lsb = lsb_props(lsb_release)?;
    let lsb_get = |key: &str| {
        lsb.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map_or("", |(_, v)| v.as_str())
    };

    let version_id = get("version_id").unwrap_or_default();
    if version_id.is_empty() {
        return Err("/etc/os-release has no VERSION_ID".into());
    }
    let version = if id == "debian" {
        // `distro.version(best=True)`: the first candidate with the most dots. Candidates parsed
        // out of a pretty name or a description have no dot unless their text has one.
        for text in [
            get("pretty_name").unwrap_or_default(),
            lsb_get("description"),
        ] {
            if text.contains('.') {
                return Err(format!("the version inside '{text}' needs distro's parser"));
            }
        }
        let mut best = version_id;
        for candidate in [lsb_get("release"), debian_version.as_str()] {
            if candidate.matches('.').count() > best.matches('.').count() {
                best = candidate;
            }
        }
        best
    } else {
        version_id
    };
    let release = get("version_codename")
        .or_else(|| get("ubuntu_codename"))
        .ok_or("/etc/os-release names no codename")?;

    let mut facts = Map::new();
    let mut put = |key: &str, value: Value| {
        facts.insert(key.to_string(), value);
    };
    put("distribution_release", Value::from(release));
    put("distribution_version", Value::from(version));
    let major = version.split('.').next().filter(|major| !major.is_empty());
    put(
        "distribution_major_version",
        Value::from(major.unwrap_or("NA")),
    );
    put("distribution_file_path", Value::from("/etc/os-release"));
    put("distribution_file_variety", Value::from("Debian"));
    put("distribution_file_parsed", Value::Bool(true));
    if data.contains("Debian") || data.contains("Raspbian") {
        put("distribution", Value::from("Debian"));
        if let Some(release) = pretty_name_release(data) {
            put("distribution_release", Value::from(release));
        }
        for line in root.lines("/etc/debian_version") {
            if let Some(minor) = minor_version(py_strip(&line)) {
                put("distribution_minor_version", Value::from(minor));
            }
        }
    } else if data.contains("Ubuntu") {
        put("distribution", Value::from("Ubuntu"));
    } else {
        return Err("/etc/os-release names neither Debian nor Ubuntu".into());
    }
    put("os_family", Value::from("Debian"));
    Ok(facts)
}

/// `distro`'s reading of `/etc/os-release`: `shlex` in POSIX mode splitting on whitespace, and
/// every word holding `=` a key, lowercased, and its value.
fn parse_os_release(text: &str) -> Result<Vec<(String, String)>, String> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Space,
        Word,
        Quote(char),
        Escape,
    }
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut state = State::Space;
    let mut escaped_from = State::Word;
    let mut chars = text.chars();
    let whitespace = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    loop {
        let next = chars.next();
        match (state, next) {
            (State::Space, None) => break,
            (State::Word, None) => {
                if !word.is_empty() || quoted {
                    words.push(std::mem::take(&mut word));
                }
                break;
            }
            (State::Quote(_), None) => return Err("/etc/os-release has an unclosed quote".into()),
            (State::Escape, None) => return Err("/etc/os-release ends in a backslash".into()),
            (State::Space | State::Word, Some(c)) if whitespace(c) || c == '#' => {
                if c == '#' {
                    for skipped in chars.by_ref() {
                        if skipped == '\n' {
                            break;
                        }
                    }
                }
                if state == State::Word && (!word.is_empty() || quoted) {
                    words.push(std::mem::take(&mut word));
                    quoted = false;
                }
                state = State::Space;
            }
            (State::Space | State::Word, Some('\\')) => {
                escaped_from = State::Word;
                state = State::Escape;
            }
            (State::Space | State::Word, Some(c @ ('"' | '\''))) => state = State::Quote(c),
            (State::Space | State::Word, Some(c)) => {
                word.push(c);
                state = State::Word;
            }
            (State::Quote(q), Some(c)) => {
                quoted = true;
                if c == q {
                    state = State::Word;
                } else if c == '\\' && q == '"' {
                    escaped_from = state;
                    state = State::Escape;
                } else {
                    word.push(c);
                }
            }
            (State::Escape, Some(c)) => {
                if let State::Quote(q) = escaped_from
                    && c != '\\'
                    && c != q
                {
                    word.push('\\');
                }
                word.push(c);
                state = escaped_from;
            }
        }
    }
    Ok(words
        .into_iter()
        .filter_map(|word| {
            let (key, value) = word.split_once('=')?;
            Some((key.to_lowercase(), value.to_string()))
        })
        .collect())
}

/// `distro`'s reading of `lsb_release -a`: each `key: value` line, the key lowercased with
/// underscores. Nothing when the command is missing or failed.
fn lsb_props(lsb_release: &LsbRelease) -> Result<Vec<(String, String)>, String> {
    let Some(out) = &lsb_release.distro else {
        return Ok(Vec::new());
    };
    if out.contains('\u{fffd}') {
        return Err("lsb_release printed something other than UTF-8".into());
    }
    Ok(splitlines(out)
        .into_iter()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((
                key.replace(' ', "_").to_lowercase(),
                py_strip(value).to_string(),
            ))
        })
        .collect())
}

/// `distro`'s `_debian_version`: the first line, right-stripped, empty when the file is missing.
/// The library reads it as ASCII and fails on anything else.
fn debian_version(host: &Host) -> Result<String, String> {
    match std::fs::read(host.root.path("/etc/debian_version")) {
        Ok(bytes) if bytes.is_ascii() => {
            let text = String::from_utf8(bytes).unwrap_or_default();
            let first = text.split_inclusive('\n').next().unwrap_or_default();
            Ok(first.trim_end_matches(super::py_space).to_string())
        }
        Ok(_) => Err("/etc/debian_version is not ASCII".into()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(format!("reading /etc/debian_version: {err}")),
    }
}

/// `(\w+)[-_](release|version)$`, matched from the start of a file name.
fn release_file_name(name: &str) -> bool {
    ["release", "version"].iter().any(|suffix| {
        name.strip_suffix(suffix)
            .and_then(|stem| stem.strip_suffix(['-', '_']))
            .is_some_and(|stem| {
                !stem.is_empty() && stem.chars().all(|c| c.is_alphanumeric() || c == '_')
            })
    })
}

/// `re.search(r"PRETTY_NAME=[^(]+ \(?([^)]+?)\)", data)`, backtracking as the expression does.
fn pretty_name_release(data: &str) -> Option<&str> {
    let bytes = data.as_bytes();
    for (at, _) in data.match_indices("PRETTY_NAME=") {
        let start = at + "PRETTY_NAME=".len();
        let end = data[start..]
            .find('(')
            .map_or(data.len(), |paren| start + paren);
        // `[^(]+` gives back characters from the right until a space follows it.
        for space in (start + 1..end.min(data.len())).rev() {
            if bytes[space] != b' ' {
                continue;
            }
            let after = space + 1;
            let with_paren = (bytes.get(after) == Some(&b'(')).then_some(after + 1);
            for group in [with_paren, Some(after)].into_iter().flatten() {
                if let Some(close) = data[group..].find(')')
                    && close > 0
                {
                    return Some(&data[group..group + close]);
                }
            }
        }
    }
    None
}

/// `re.search(r'(\d+)\.(\d+)', line)`, its second group.
fn minor_version(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if !bytes[at].is_ascii_digit() {
            at += 1;
            continue;
        }
        let run_end = at
            + bytes[at..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
        if bytes.get(run_end) == Some(&b'.')
            && bytes.get(run_end + 1).is_some_and(u8::is_ascii_digit)
        {
            let minor_end = run_end
                + 1
                + bytes[run_end + 1..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
            return Some(&line[run_end + 1..minor_end]);
        }
        at = run_end;
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::tests::{DEBIAN_OS_RELEASE, FakeRoot, UBUNTU_OS_RELEASE, debian_root, probe};
    use super::*;

    fn distribution(fake: &FakeRoot) -> Result<Value, String> {
        distribution_with(fake, None)
    }

    fn distribution_with(fake: &FakeRoot, distro: Option<&str>) -> Result<Value, String> {
        let root = fake.root();
        let mut probe = probe();
        probe.distro = distro.map(str::to_string);
        let host = fake.host(&root, &probe);
        let lsb = LsbRelease::run(host.root, &host.env, super::super::unbounded()).unwrap();
        collect(&host, &lsb).map(Value::Object)
    }

    /// A system `distro` other than 1.9 hands back: `ansible.module_utils.distro` imports it
    /// before its bundled copy, and 1.5 ranks versions without `/etc/debian_version` (a Debian
    /// 11 host would be `11` there, `11.11` here).
    ///
    /// What would make this red: the probe's `distro` ignored.
    #[test]
    fn a_system_distro_other_than_1_9_hands_back() {
        let fake = debian_root("system-distro");
        assert!(distribution_with(&fake, Some("1.9.0")).is_ok());
        let reason = distribution_with(&fake, Some("1.5.0")).unwrap_err();
        assert!(reason.contains("1.5.0"), "{reason}");
        assert!(
            distribution_with(&fake, Some("")).is_err(),
            "a version nobody could read"
        );
    }

    /// Ubuntu 24.04's `/etc/os-release`, sanitised, as target and gen measured it.
    ///
    /// What would make this red: the release read from `VERSION`'s `(Noble Numbat)` rather than
    /// `VERSION_CODENAME`, or the variety spelled `Ubuntu` (the reference names the file entry,
    /// `Debian`).
    #[test]
    fn ubuntu_is_read_like_the_reference() {
        let fake = FakeRoot::new("ubuntu");
        fake.write("/etc/os-release", UBUNTU_OS_RELEASE)
            .write("/etc/debian_version", "trixie/sid\n")
            .write("/etc/lsb-release", "DISTRIB_ID=Ubuntu\n");
        assert_eq!(
            distribution(&fake).unwrap(),
            json!({
                "distribution_release": "noble",
                "distribution_version": "24.04",
                "distribution_major_version": "24",
                "distribution_file_path": "/etc/os-release",
                "distribution_file_variety": "Debian",
                "distribution_file_parsed": true,
                "distribution": "Ubuntu",
                "os_family": "Debian",
            })
        );
    }

    /// Debian 12, sanitised: the version is `/etc/debian_version`'s because it has the most dots,
    /// the release comes from `PRETTY_NAME`, the minor version from `debian_version`.
    ///
    /// What would make this red: `12` kept as the version, or `distribution_minor_version`
    /// missing.
    #[test]
    fn debian_is_read_like_the_reference() {
        let fake = debian_root("debian");
        assert_eq!(
            distribution(&fake).unwrap(),
            json!({
                "distribution_release": "bookworm",
                "distribution_version": "12.7",
                "distribution_major_version": "12",
                "distribution_file_path": "/etc/os-release",
                "distribution_file_variety": "Debian",
                "distribution_file_parsed": true,
                "distribution": "Debian",
                "distribution_minor_version": "7",
                "os_family": "Debian",
            })
        );
    }

    /// Outside Debian and Ubuntu, or where the reference's walk would stop earlier, hand back.
    ///
    /// What would make this red: `ID=fedora` answered, or a leftover `/etc/redhat-release` on an
    /// Ubuntu host ignored when the reference names the host after it.
    #[test]
    fn another_distribution_hands_back() {
        let fake = FakeRoot::new("fedora");
        assert!(distribution(&fake).is_err(), "no os-release");
        fake.write(
            "/etc/os-release",
            "NAME=\"Fedora Linux\"\nVERSION_ID=40\nID=fedora\nVERSION_CODENAME=\"\"\n",
        );
        let reason = distribution(&fake).unwrap_err();
        assert!(reason.contains("fedora"), "{reason}");
        fake.write("/etc/os-release", UBUNTU_OS_RELEASE);
        assert!(distribution(&fake).is_ok());
        for leftover in ["/etc/redhat-release", "/etc/fedora-release"] {
            fake.write(leftover, "Fedora release 40 (Forty)\n");
            assert!(distribution(&fake).is_err(), "{leftover}");
            std::fs::remove_file(fake.0.join(leftover.trim_start_matches('/'))).unwrap();
        }
        fake.write("/etc/arch-release", "");
        assert!(distribution(&fake).is_err(), "an empty arch-release counts");
        std::fs::remove_file(fake.0.join("etc/arch-release")).unwrap();
        fake.write(
            "/etc/os-release",
            &UBUNTU_OS_RELEASE
                .replace("VERSION_CODENAME=noble\n", "")
                .replace("UBUNTU_CODENAME=noble\n", ""),
        );
        assert!(distribution(&fake).is_err(), "no codename");
        fake.write(
            "/etc/os-release",
            DEBIAN_OS_RELEASE
                .replace("12 (bookworm)\"\nNAME", "12.1 (bookworm)\"\nNAME")
                .as_str(),
        );
        assert!(
            distribution(&fake).is_err(),
            "a dotted pretty name on Debian"
        );
    }

    /// Python's `shlex` in POSIX mode: quotes join into a word, a backslash in double quotes
    /// escapes only a quote or a backslash, `#` ends a word and its line.
    #[test]
    fn os_release_is_split_like_shlex() {
        let parsed = parse_os_release(
            "A=\"x y\"z\nB='q \\ r'\nC=\"a\\\"b\\$c\\\\\"\nD=u#rest=1\n# E=no\nF=\"\"\nG=a\\ b\n",
        )
        .unwrap();
        let expected: Vec<(String, String)> = [
            ("a", "x yz"),
            ("b", "q \\ r"),
            ("c", "a\"b\\$c\\"),
            ("d", "u"),
            ("f", ""),
            ("g", "a b"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(parsed, expected);
        assert!(parse_os_release("A=\"open\n").is_err());
    }

    /// The two regular expressions, on the lines they meet and on their edge cases.
    #[test]
    fn the_release_and_minor_expressions_match_like_python() {
        assert_eq!(
            pretty_name_release("PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nNAME=x"),
            Some("bookworm")
        );
        assert_eq!(
            pretty_name_release(
                "PRETTY_NAME=\"Debian GNU/Linux trixie/sid\"\nVERSION=\"13 (trixie)\""
            ),
            Some("trixie"),
            "[^(]+ runs across lines to the next parenthesis"
        );
        assert_eq!(pretty_name_release("PRETTY_NAME=\"Debian\"\n"), None);
        assert_eq!(pretty_name_release("PRETTY_NAME=a b)"), Some("b"));
        assert_eq!(minor_version("12.7"), Some("7"));
        assert_eq!(minor_version("v 1.23.4"), Some("23"));
        assert_eq!(minor_version("trixie/sid"), None);
        assert_eq!(minor_version("12. 3.4"), Some("4"));
        assert!(release_file_name("fedora-release"));
        assert!(release_file_name("debian_version"));
        assert!(!release_file_name("-release"));
        assert!(!release_file_name("os.release"));
    }
}
