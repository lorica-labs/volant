// SPDX-License-Identifier: GPL-3.0-or-later
//! The version classes the `version` test picks with `strict` and `version_type`, beside the
//! default `LooseVersion`: distutils' `StrictVersion`, ansible-core's `SemanticVersion` and the
//! PEP 440 `Version` of `packaging`. Each parses and orders as the reference's does, and refuses
//! a text it cannot parse with the reference's words.

use std::cmp::Ordering;
use std::sync::LazyLock;

use regex::Regex;

/// A value with an end below and above every other, as `packaging` orders a missing segment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Bound<T> {
    Low,
    At(T),
    High,
}

/// `StrictVersion`: `1.2`, `1.2.3`, with an optional `a` or `b` pre-release.
/// A strict version: its release, then its pre-release.
type StrictKey = ([u64; 3], Bound<(char, u64)>);

fn strict(text: &str) -> Result<StrictKey, String> {
    static FORM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(\d+)\.(\d+)(?:\.(\d+))?(?:([ab])(\d+))?$").expect("valid"));
    let caps = FORM
        .captures(text)
        .ok_or_else(|| format!("invalid version number '{text}'"))?;
    let number = |i: usize| caps.get(i).map_or(Ok(0), |m| m.as_str().parse::<u64>());
    let invalid = |_| format!("invalid version number '{text}'");
    let release = [
        number(1).map_err(invalid)?,
        number(2).map_err(invalid)?,
        number(3).map_err(invalid)?,
    ];
    let pre = match caps.get(4) {
        // No pre-release sorts after every pre-release of the same release.
        None => Bound::High,
        Some(letter) => Bound::At((
            letter.as_str().chars().next().unwrap_or('a'),
            number(5).map_err(invalid)?,
        )),
    };
    Ok((release, pre))
}

/// One identifier of a semantic version's pre-release: a number sorts before any text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    Number(u128),
    Text(String),
}

/// A semantic version: its core, then its pre-release.
type SemanticKey = ([u128; 3], Bound<Vec<Identifier>>);

/// `SemanticVersion`: the core, then the pre-release; build metadata takes no part.
fn semantic(text: &str) -> Result<SemanticKey, String> {
    static FORM: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$",
        )
        .expect("valid")
    });
    let invalid = || format!("invalid semantic version '{text}'");
    let caps = FORM.captures(text).ok_or_else(invalid)?;
    let number = |i: usize| caps[i].parse::<u128>().map_err(|_| invalid());
    let core = [number(1)?, number(2)?, number(3)?];
    let pre = caps.get(4).map_or(Bound::High, |m| {
        Bound::At(
            m.as_str()
                .split('.')
                .map(|part| {
                    part.parse::<u128>()
                        .map_or_else(|_| Identifier::Text(part.to_owned()), Identifier::Number)
                })
                .collect(),
        )
    });
    Ok((core, pre))
}

/// One segment of a PEP 440 local version: text sorts before any number.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Local {
    Text(String),
    Number(u128),
}

type Pep440Key = (
    u128,
    Vec<u128>,
    Bound<(u8, u128)>,
    Bound<u128>,
    Bound<u128>,
    Bound<Vec<Local>>,
);

/// `packaging.version.Version`, ordered by `packaging`'s `_cmpkey`.
fn pep440(text: &str) -> Result<Pep440Key, String> {
    static FORM: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^\s*v?(?:([0-9]+)!)?([0-9]+(?:\.[0-9]+)*)(?:[-_.]?(alpha|a|beta|b|preview|pre|c|rc)[-_.]?([0-9]+)?)?(?:-([0-9]+)|[-_.]?(post|rev|r)[-_.]?([0-9]+)?)?(?:[-_.]?(dev)[-_.]?([0-9]+)?)?(?:\+([a-z0-9]+(?:[-_.][a-z0-9]+)*))?\s*$",
        )
        .expect("valid")
    });
    let invalid = || format!("Invalid version: '{text}'");
    let caps = FORM.captures(text).ok_or_else(invalid)?;
    let number = |i: usize| -> Result<Option<u128>, String> {
        caps.get(i)
            .map(|m| m.as_str().parse::<u128>().map_err(|_| invalid()))
            .transpose()
    };
    let epoch = number(1)?.unwrap_or(0);
    let mut release = caps[2]
        .split('.')
        .map(|p| p.parse::<u128>().map_err(|_| invalid()))
        .collect::<Result<Vec<_>, _>>()?;
    while release.len() > 1 && release.last() == Some(&0) {
        release.pop();
    }
    let pre = caps.get(3).map(|letter| {
        let rank = match letter.as_str().to_ascii_lowercase().as_str() {
            "a" | "alpha" => 0,
            "b" | "beta" => 1,
            _ => 2,
        };
        (rank, number(4).ok().flatten().unwrap_or(0))
    });
    let post = match (number(5)?, caps.get(6)) {
        (Some(n), _) => Some(n),
        (None, Some(_)) => Some(number(7)?.unwrap_or(0)),
        (None, None) => None,
    };
    let dev = caps.get(8).map(|_| number(9).ok().flatten().unwrap_or(0));
    let local = caps.get(10).map(|m| {
        m.as_str()
            .split(['-', '_', '.'])
            .map(|part| {
                part.parse::<u128>()
                    .map_or_else(|_| Local::Text(part.to_ascii_lowercase()), Local::Number)
            })
            .collect::<Vec<_>>()
    });
    let pre = match (pre, post, dev) {
        (None, None, Some(_)) => Bound::Low,
        (None, _, _) => Bound::High,
        (Some(p), _, _) => Bound::At(p),
    };
    Ok((
        epoch,
        release,
        pre,
        post.map_or(Bound::Low, Bound::At),
        dev.map_or(Bound::High, Bound::At),
        local.map_or(Bound::Low, Bound::At),
    ))
}

/// The order of `a` and `b` under `version_type` (`strict`, `semver`, `semantic` or `pep440`),
/// or the error the reference's class raises.
pub(super) fn compare(version_type: &str, a: &str, b: &str) -> Result<Ordering, String> {
    match version_type {
        "strict" => Ok(strict(a)?.cmp(&strict(b)?)),
        "semver" | "semantic" => Ok(semantic(a)?.cmp(&semantic(b)?)),
        _ => Ok(pep440(a)?.cmp(&pep440(b)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::compare;
    use std::cmp::Ordering::{Equal, Greater, Less};

    /// Measured on ansible-core 2.19.12 through the `version` test.
    #[rstest::rstest]
    #[case("strict", "1.2.3", "1.2", Greater)]
    #[case("strict", "1.0a1", "1.0", Less)]
    #[case("strict", "1.0b1", "1.0a2", Greater)]
    #[case("strict", "1.0", "1.0.0", Equal)]
    #[case("semver", "1.0.0-beta", "1.0.0", Less)]
    #[case("semver", "1.0.0-alpha.1", "1.0.0-alpha.beta", Less)]
    #[case("semver", "1.0.0+b1", "1.0.0", Equal)]
    #[case("semver", "1.0.0-2", "1.0.0-10", Less)]
    #[case("pep440", "1.0rc1", "1.0.0", Less)]
    #[case("pep440", "1.0.dev1", "1.0a1", Less)]
    #[case("pep440", "1.0.post1", "1.0", Greater)]
    #[case("pep440", "1.0+local.1", "1.0", Greater)]
    #[case("pep440", "1!0.1", "2.0", Greater)]
    #[case("pep440", "1.0", "1.0.0", Equal)]
    #[case("pep440", "1.0rc1", "1.0c1", Equal)]
    #[case("pep440", "1.0+abc", "1.0+1", Less)]
    #[case("pep440", "1.0a1.post2", "1.0a1", Greater)]
    #[case("pep440", "1.0.post1.dev1", "1.0.post1", Less)]
    fn versions_order_as_the_reference_s_classes(
        #[case] kind: &str,
        #[case] a: &str,
        #[case] b: &str,
        #[case] order: std::cmp::Ordering,
    ) {
        assert_eq!(compare(kind, a, b), Ok(order));
    }

    #[rstest::rstest]
    #[case("strict", "1.2.3.4", "invalid version number '1.2.3.4'")]
    #[case("semver", "1.2", "invalid semantic version '1.2'")]
    #[case("pep440", "1.2x", "Invalid version: '1.2x'")]
    fn a_text_the_class_cannot_read_is_refused(
        #[case] kind: &str,
        #[case] text: &str,
        #[case] said: &str,
    ) {
        assert_eq!(compare(kind, text, "1.0.0"), Err(said.to_owned()));
    }
}
