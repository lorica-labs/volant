// SPDX-License-Identifier: GPL-3.0-or-later
//! The configured `deb` sources, and the `Origin` of their lists.

use std::collections::HashMap;

use super::dpkg::field;
use super::imp::{LISTS, read_text};
use crate::natives::runtime::Root;

/// One configured `deb` source: the prefix of its files in apt's lists, and the names of the
/// `Packages` files apt keeps for it.
#[derive(Debug, PartialEq)]
pub(super) struct Source {
    pub(super) prefix: String,
    pub(super) packages: Vec<String>,
}

/// Every `deb` source of `sources.list` and `sources.list.d`, one-line and deb822 alike.
///
/// apt refuses the whole list when two entries for one URI and suite, `deb-src` ones
/// included, set `Signed-By` or `Trusted` differently (apt 2.8 counts an absent one as a
/// value): the module then fails, and the native hands back.
pub(super) fn sources(
    root: &Root,
    native: &str,
    foreign: &[String],
) -> Result<Vec<Source>, String> {
    let mut default_archs = vec![native.to_string()];
    default_archs.extend(foreign.iter().cloned());
    let mut entries = Vec::new();
    if root.is_file("/etc/apt/sources.list") {
        one_line(&read_text(root, "/etc/apt/sources.list")?, &mut entries)?;
    }
    if let Ok(dir) = std::fs::read_dir(root.path("/etc/apt/sources.list.d")) {
        for entry in dir {
            let entry = entry.map_err(|err| format!("sources.list.d cannot be read: {err}"))?;
            let name = entry.file_name();
            let name = name.to_str().ok_or("a source file's name is not UTF-8")?;
            let one = name.ends_with(".list");
            if !one && !name.ends_with(".sources") {
                continue;
            }
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
            {
                return Err(format!("the source file {name} has a name apt may skip"));
            }
            let text = read_text(root, &format!("/etc/apt/sources.list.d/{name}"))?;
            if one {
                one_line(&text, &mut entries)?;
            } else {
                deb822(&text, &mut entries)?;
            }
        }
    }
    let mut trust: HashMap<String, &[Option<String>; 2]> = HashMap::new();
    let mut sources = Vec::new();
    for entry in &entries {
        let prefix = prefix(&entry.uri, &entry.suite)?;
        if trust
            .insert(prefix.clone(), &entry.trust)
            .is_some_and(|seen| *seen != entry.trust)
        {
            return Err("a source is given two different Signed-By or Trusted".into());
        }
        if !entry.binary {
            continue;
        }
        let archs = entry.archs.as_ref().unwrap_or(&default_archs);
        let mut packages = Vec::new();
        for component in &entry.components {
            if !component
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
            {
                return Err("a component is not a plain name".into());
            }
            for arch in archs {
                packages.push(format!("{prefix}{component}_binary-{arch}_Packages"));
            }
        }
        if packages.is_empty() {
            return Err("a source names no component".into());
        }
        sources.push(Source { prefix, packages });
    }
    Ok(sources)
}

struct Entry {
    /// `deb`, rather than `deb-src`.
    binary: bool,
    uri: String,
    suite: String,
    components: Vec<String>,
    archs: Option<Vec<String>>,
    /// `Signed-By` and `Trusted`, as written.
    trust: [Option<String>; 2],
}

/// Options that change which files apt reads, or where: the native does not follow them.
const OPTIONS_HANDED_BACK: [&str; 7] = [
    "targets",
    "architectures-add",
    "architectures-remove",
    "inrelease-path",
    "snapshot",
    "include",
    "exclude",
];

/// `sources.list`'s one-line form: `deb [options] uri suite component...`.
fn one_line(text: &str, entries: &mut Vec<Entry>) -> Result<(), String> {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default();
        let mut words = line.split_whitespace().peekable();
        let Some(kind) = words.next() else {
            continue;
        };
        let mut options = Vec::new();
        if words.peek().is_some_and(|word| word.starts_with('[')) {
            for word in words.by_ref() {
                let done = word.ends_with(']');
                let word = word.trim_start_matches('[').trim_end_matches(']');
                options.extend(word.split_whitespace().map(str::to_string));
                if done {
                    break;
                }
            }
        }
        let words: Vec<&str> = words.collect();
        let binary = match kind {
            "deb" => true,
            "deb-src" => false,
            _ => return Err("a source type is not deb".into()),
        };
        let [uri, suite, components @ ..] = &words[..] else {
            return Err("a source line is incomplete".into());
        };
        let mut archs = None;
        let mut trust = [None, None];
        // apt reads no `enabled` option on a one-line entry: `[enabled=no]` still fetches.
        for option in options {
            let (key, value) = option.split_once('=').unwrap_or((&option, ""));
            let key = key.to_ascii_lowercase();
            match key.as_str() {
                "arch" => archs = Some(value.split(',').map(str::to_string).collect()),
                "signed-by" => trust[0] = Some(value.to_string()),
                "trusted" => trust[1] = Some(value.to_string()),
                "target" | "inrelease-path" | "snapshot" | "include" | "exclude" => {
                    return Err(format!("the source option {key} changes its lists"));
                }
                _ if key.ends_with('+') || key.ends_with('-') => {
                    return Err(format!("the source option {key} changes its lists"));
                }
                _ => {}
            }
        }
        entries.push(Entry {
            binary,
            uri: (*uri).to_string(),
            suite: (*suite).to_string(),
            components: components.iter().map(|c| (*c).to_string()).collect(),
            archs,
            trust,
        });
    }
    Ok(())
}

/// apt's `StringToBool` without a default: an integer 0 or 1 as `strtol` reads the whole
/// text, then its words for no and yes in any case. Anything else takes the caller's
/// default in apt, which the native does not guess: `None`.
pub(super) fn apt_bool(text: &str) -> Option<bool> {
    let (negative, body) = match text.strip_prefix(['+', '-']) {
        Some(rest) => (text.starts_with('-'), rest),
        None => (false, text),
    };
    // Rust's parsers take a sign of their own, which `strtol` would not after the first.
    let number = if body.starts_with(['+', '-']) {
        None
    } else if let Some(hex) = body.strip_prefix("0x").or(body.strip_prefix("0X")) {
        u128::from_str_radix(hex, 16)
            .ok()
            .filter(|_| !hex.starts_with(['+', '-']))
    } else if body.len() > 1 && body.starts_with('0') {
        u128::from_str_radix(body, 8).ok()
    } else {
        body.parse::<u128>().ok()
    };
    match number {
        Some(0) => return Some(false),
        Some(1) if !negative => return Some(true),
        _ => {}
    }
    let word = text.to_ascii_lowercase();
    if ["no", "false", "without", "off", "disable"].contains(&word.as_str()) {
        Some(false)
    } else if ["yes", "true", "with", "on", "enable"].contains(&word.as_str()) {
        Some(true)
    } else {
        None
    }
}

/// The deb822 form of `*.sources`: one entry per type, URI and suite of each paragraph.
fn deb822(text: &str, entries: &mut Vec<Entry>) -> Result<(), String> {
    let mut paragraphs = vec![String::new()];
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let current = paragraphs.last_mut().expect("never empty");
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(String::new());
            }
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    for stanza in paragraphs.iter().filter(|stanza| !stanza.is_empty()) {
        let stanza = stanza.as_str();
        for line in stanza.split('\n') {
            if let Some((key, _)) = line.split_once(':')
                && !line.starts_with([' ', '\t'])
                && OPTIONS_HANDED_BACK.contains(&key.trim().to_ascii_lowercase().as_str())
            {
                return Err(format!("the source field {key} changes its lists"));
            }
        }
        let words = |name: &str| -> Vec<String> {
            field(stanza, name).map_or(Vec::new(), |value| {
                value.split_whitespace().map(str::to_string).collect()
            })
        };
        if let Some(value) = field(stanza, "Enabled") {
            match apt_bool(value) {
                Some(false) => continue,
                Some(true) => {}
                None => {
                    return Err("the source field Enabled is not a boolean".into());
                }
            }
        }
        // apt refuses the whole list over a paragraph missing one of these, and the module
        // then fails.
        if let Some(missing) = ["Types", "URIs", "Suites"]
            .into_iter()
            .find(|&name| words(name).is_empty())
        {
            return Err(format!("a deb822 source paragraph has no {missing}"));
        }
        let archs = field(stanza, "Architectures").map(|_| words("Architectures"));
        let trust = ["Signed-By", "Trusted"].map(|name| field(stanza, name).map(str::to_string));
        for kind in words("Types") {
            let binary = match kind.as_str() {
                "deb" => true,
                "deb-src" => false,
                _ => return Err("a source type is not deb".into()),
            };
            for uri in words("URIs") {
                for suite in words("Suites") {
                    entries.push(Entry {
                        binary,
                        uri: uri.clone(),
                        suite,
                        components: words("Components"),
                        archs: archs.clone(),
                        trust: trust.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// The schemes other than http(s) apt's methods take, named when a source uses one.
const OTHER_SCHEMES: &[&str] = &[
    "mirror",
    "mirror+file",
    "mirror+http",
    "mirror+https",
    "file",
    "cdrom",
    "copy",
    "ftp",
    "tor+http",
    "tor+https",
    "rsh",
    "ssh",
];

/// apt's `URItoFileName` of `<uri>/dists/<suite>/`: the URI without its scheme, `/` turned
/// into `_`. Only plain http(s) URIs and suites, which it leaves otherwise alone.
pub(super) fn prefix(uri: &str, suite: &str) -> Result<String, String> {
    let rest = uri
        .strip_prefix("http://")
        .or_else(|| uri.strip_prefix("https://"))
        .ok_or_else(|| {
            // Only a scheme on this list, which the code owns, is named: whatever the file
            // holds past it, or in its place, can carry credentials.
            let scheme = uri.split_once(':').map(|(scheme, _)| scheme);
            match OTHER_SCHEMES.iter().find(|known| Some(**known) == scheme) {
                Some(known) => format!("the source scheme {known} is not http or https"),
                None => "a source whose scheme is not http or https".to_string(),
            }
        })?;
    let plain = |text: &str, extra: &str| {
        !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
    };
    if !plain(rest, ".-/:") || rest.contains("//") {
        return Err("a source URI is not a plain one".into());
    }
    if !plain(suite, ".-/") || suite.ends_with('/') {
        return Err("a suite is not a plain name".into());
    }
    let rest = rest.trim_end_matches('/');
    Ok(format!(
        "{}_dists_{}_",
        rest.replace('/', "_"),
        suite.replace('/', "_")
    ))
}

/// The `Origin` of the `InRelease` (else `Release`) beside a source's lists: what python-apt
/// gives as `origin` for a version found there.
pub(super) fn origin(root: &Root, prefix: &str) -> Result<String, String> {
    let text = ["InRelease", "Release"]
        .iter()
        .map(|name| format!("{LISTS}/{prefix}{name}"))
        .find(|path| root.is_file(path))
        .ok_or_else(|| format!("{prefix} has lists and no Release"))
        .and_then(|path| read_text(root, &path))?;
    let body = match text.strip_prefix("-----BEGIN PGP SIGNED MESSAGE-----\n") {
        Some(signed) => signed
            .split_once("\n\n")
            .map(|(_, body)| body)
            .ok_or_else(|| format!("{prefix}InRelease has no body"))?,
        None => &text,
    };
    let first = body.split("\n\n").next().unwrap_or_default();
    field(first, "Origin")
        .map(str::to_string)
        .ok_or_else(|| format!("{prefix} has a Release without an Origin"))
}
