// SPDX-License-Identifier: GPL-3.0-or-later
//! Collection versions and the ranges a requirement gives, as ansible-galaxy 2.19.12 compares
//! them (`dependency_resolution/versioning.py`, `ansible.utils.version.SemanticVersion`).

use std::cmp::Ordering;

/// A semantic version: three numbers, a pre-release and build metadata. Build metadata does not
/// take part in the order.
#[derive(Debug, Clone)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Version {
    /// A strict `major.minor.patch[-pre][+build]`, or `None`.
    pub fn parse(text: &str) -> Option<Self> {
        let (rest, _build) = text.split_once('+').map_or((text, ""), |(a, b)| (a, b));
        let (core, pre) = rest.split_once('-').map_or((rest, ""), |(a, b)| (a, b));
        let numbers: Vec<u64> = core
            .split('.')
            .map(|n| {
                (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| n.parse().ok())
                    .flatten()
            })
            .collect::<Option<_>>()?;
        let [major, minor, patch] = numbers[..] else {
            return None;
        };
        if rest.contains('-') && pre.is_empty() {
            return None;
        }
        Some(Self {
            core: [major, minor, patch],
            pre: if pre.is_empty() {
                Vec::new()
            } else {
                pre.split('.').map(str::to_string).collect()
            },
        })
    }

    /// A version as a requirement writes it, read loosely: missing numbers are zero, so `2` is
    /// `2.0.0` (`SemanticVersion.from_loose_version`).
    pub fn loose(text: &str) -> Option<Self> {
        if let Some(strict) = Self::parse(text) {
            return Some(strict);
        }
        let end = text.find(['-', '+']).unwrap_or(text.len());
        let mut numbers = text[..end].split('.').map(|n| n.parse::<u64>().ok());
        let mut core = [0; 3];
        for slot in &mut core {
            match numbers.next() {
                Some(Some(n)) => *slot = n,
                Some(None) => return None,
                None => break,
            }
        }
        if numbers.next().is_some() {
            return None;
        }
        let pre = text[end..]
            .strip_prefix('-')
            .map(|p| {
                p.split('+')
                    .next()
                    .unwrap_or(p)
                    .split('.')
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Some(Self { core, pre })
    }

    pub fn is_pre_release(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

/// Whether a strict pre-release: anything else counts as a release.
pub fn is_pre_release(version: &str) -> bool {
    Version::parse(version).is_some_and(|v| v.is_pre_release())
}

/// Whether `version` meets every comma-separated part of `range`: `*`, a bare version, or one of
/// `==`, `=`, `!=`, `>=`, `>`, `<=`, `<` before it (`meets_requirements`). A part that cannot be
/// read is not met.
pub fn meets(version: &str, range: &str) -> bool {
    range.split(',').all(|part| {
        let width = if part.len() > 1 && part.as_bytes()[1] == b'=' {
            2
        } else {
            1
        };
        let (op, wanted) = match part.get(..width) {
            Some(op @ ("!=" | "==" | "=" | ">=" | ">" | "<=" | "<")) => (op, &part[width..]),
            _ => ("==", part),
        };
        if wanted == "*" || version == "*" {
            return true;
        }
        let (Some(have), Some(want)) = (Version::parse(version), Version::loose(wanted)) else {
            return false;
        };
        match op {
            "!=" => have != want,
            ">=" => have >= want,
            ">" => have > want,
            "<=" => have <= want,
            "<" => have < want,
            _ => have == want,
        }
    })
}

/// Whether `range` names one version exactly: a bare version or `==`.
pub fn is_pin(range: &str) -> bool {
    !range.contains(',') && range != "*" && !range.starts_with(['<', '>', '!'])
}
