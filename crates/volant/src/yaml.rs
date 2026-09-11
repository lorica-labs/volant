// SPDX-License-Identifier: GPL-3.0-or-later
//! YAML loading shared by playbooks, variable files and inventories, with the PyYAML habits
//! Ansible content relies on (`yes`/`no` booleans, leading-zero and underscore-grouped
//! integers) and the tags Ansible adds (`!vault`, `!unsafe`).

use anyhow::{Context, anyhow, bail};
use saphyr::{LoadableYamlNode, Mapping, MarkedYaml, Scalar, Yaml, YamlData};
use serde_json::{Map, Value};

/// Loads every document of `text`. Errors name `source`.
pub fn load<'a>(text: &'a str, source: &str) -> anyhow::Result<Vec<Yaml<'a>>> {
    let docs =
        MarkedYaml::load_from_str(text).with_context(|| format!("{source}: invalid YAML"))?;
    Ok(docs.into_iter().map(|doc| lower(doc, text)).collect())
}

/// Drops the source positions `MarkedYaml` tracks once loading is done, applying PyYAML's
/// scalar resolution along the way where it's confirmed to diverge from saphyr's YAML 1.2 core
/// schema (see `pyyaml_scalar`). Every other function in this module keeps working on the
/// plain `Yaml` tree it already expected; only `load` needs to know this happened.
fn lower<'a>(node: MarkedYaml<'a>, text: &'a str) -> Yaml<'a> {
    match node.data {
        YamlData::Value(scalar) => {
            let start = char_to_byte(text, node.span.start.index());
            let end = char_to_byte(text, node.span.end.index());
            let raw = text.get(start..end).unwrap_or_default();
            Yaml::Value(pyyaml_scalar(scalar, raw))
        }
        YamlData::Sequence(items) => {
            Yaml::Sequence(items.into_iter().map(|n| lower(n, text)).collect())
        }
        YamlData::Mapping(map) => {
            let mut out = Mapping::new();
            for (k, v) in map {
                out.insert(lower(k, text), lower(v, text));
            }
            Yaml::Mapping(out)
        }
        YamlData::Tagged(tag, inner) => Yaml::Tagged(tag, Box::new(lower(*inner, text))),
        YamlData::Alias(id) => Yaml::Alias(id),
        YamlData::Representation(v, style, tag) => Yaml::Representation(v, style, tag),
        YamlData::BadValue => Yaml::BadValue,
    }
}

/// `text`'s byte offset for the `chars`-th character in it: saphyr's markers count characters,
/// while `str` slicing needs bytes.
fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(i, _)| i)
}

/// Every spelling PyYAML reads as `true`. The single source for both the scalar resolver
/// below and [`bool_from_str`]; the `bool` filter's set is a different one and lives with the
/// filter (see [`crate::template`]).
pub const PYYAML_TRUE: &[&str] = &[
    "true", "True", "TRUE", "yes", "Yes", "YES", "on", "On", "ON",
];

/// Every spelling PyYAML reads as `false`, the counterpart of [`PYYAML_TRUE`].
pub const PYYAML_FALSE: &[&str] = &[
    "false", "False", "FALSE", "no", "No", "NO", "off", "Off", "OFF",
];

/// Re-resolves a scalar the way ansible-core's PyYAML (YAML 1.1) would. `raw` is the node's
/// exact source text; quoting it, or writing it as a block scalar, makes `raw` start with
/// something other than the bare word or digits, so none of the spellings below match and
/// `scalar` is returned untouched — matching PyYAML, which also only resolves *plain* scalars,
/// so a deliberately quoted `"yes"` stays the string a playbook wrote.
///
/// The last arm is the general form of what used to be a list of one-off exceptions: whenever
/// PyYAML's own resolvers refuse a plain scalar that saphyr's YAML 1.2 core schema typed as a
/// number, PyYAML leaves a string, so we do too. Measured against ansible-core 2.19.12, that
/// covers `08` (no valid octal digit), `0o10` (the `0o` prefix is YAML 1.2 only) and `1e3` or
/// `1e+3` (PyYAML's float needs a `.` in the mantissa).
fn pyyaml_scalar<'a>(scalar: Scalar<'a>, raw: &'a str) -> Scalar<'a> {
    if PYYAML_TRUE.contains(&raw) {
        return Scalar::Boolean(true);
    }
    if PYYAML_FALSE.contains(&raw) {
        return Scalar::Boolean(false);
    }
    if let Some(i) = pyyaml_int(raw) {
        return Scalar::Integer(i);
    }
    if let Some(f) = pyyaml_float(raw) {
        return Scalar::FloatingPoint(f.into());
    }
    if matches!(scalar, Scalar::Integer(_) | Scalar::FloatingPoint(_)) {
        return Scalar::String(raw.into());
    }
    scalar
}

/// PyYAML's YAML 1.1 integer spellings: an optional sign, then `0b` binary, `0x` hexadecimal, a
/// bare leading `0` for octal, or a decimal starting `1`-`9` (or the single digit `0`). Every
/// digit run may carry `_` separators. `0o10` is deliberately absent: that prefix arrived with
/// YAML 1.2 and PyYAML leaves it a string.
fn pyyaml_int(raw: &str) -> Option<i64> {
    let (negative, rest) = split_sign(raw);
    let value = if let Some(digits) = rest.strip_prefix("0b") {
        digit_run(digits, 2, |b| matches!(b, b'0' | b'1'))?
    } else if let Some(digits) = rest.strip_prefix("0x") {
        digit_run(digits, 16, |b| b.is_ascii_hexdigit())?
    } else if rest == "0" {
        0
    } else if let Some(digits) = rest.strip_prefix('0') {
        digit_run(digits, 8, |b| matches!(b, b'0'..=b'7'))?
    } else if matches!(rest.as_bytes().first(), Some(b'1'..=b'9')) {
        digit_run(rest, 10, |b| b.is_ascii_digit())?
    } else {
        return None;
    };
    Some(if negative { -value } else { value })
}

/// One run of `radix` digits, `_` separators allowed anywhere in it, as an `i64`. A run that is
/// empty, holds a digit `ok` refuses, or overflows is no match at all: the caller then falls
/// back to what saphyr resolved.
fn digit_run(digits: &str, radix: u32, ok: impl Fn(u8) -> bool) -> Option<i64> {
    if digits.is_empty() || !digits.bytes().all(|b| ok(b) || b == b'_') {
        return None;
    }
    i64::from_str_radix(&digits.replace('_', ""), radix).ok()
}

/// PyYAML's YAML 1.1 float spellings: `.inf`, `.nan`, or a mantissa that must contain a `.`,
/// optionally followed by an exponent whose sign PyYAML **requires**. Measured against
/// ansible-core 2.19.12: `1.0e+3` is the float 1000.0 while `1.0e3`, `1e3` and `1e+3` are all
/// strings there.
fn pyyaml_float(raw: &str) -> Option<f64> {
    let (negative, rest) = split_sign(raw);
    if matches!(rest, ".nan" | ".NaN" | ".NAN") {
        // PyYAML's not-a-number spelling carries no sign.
        return (!negative).then_some(f64::NAN);
    }
    let value = if matches!(rest, ".inf" | ".Inf" | ".INF") {
        f64::INFINITY
    } else {
        let (mantissa, exponent) = match rest.split_once(['e', 'E']) {
            Some((mantissa, exponent)) => (mantissa, Some(exponent)),
            None => (rest, None),
        };
        let (whole, fraction) = mantissa.split_once('.')?;
        let digits = |run: &str| run.bytes().all(|b| b.is_ascii_digit() || b == b'_');
        match whole.as_bytes().first() {
            // `.5`: nothing before the dot means at least one digit after it.
            None if fraction.is_empty() || !digits(fraction) => return None,
            Some(b'0'..=b'9') if !digits(whole) || !digits(fraction) => return None,
            None | Some(b'0'..=b'9') => {}
            _ => return None,
        }
        if let Some(exponent) = exponent {
            let signed = exponent.strip_prefix('+').or(exponent.strip_prefix('-'))?;
            if signed.is_empty() || !signed.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
        }
        rest.replace('_', "").parse().ok()?
    };
    Some(if negative { -value } else { value })
}

/// Splits an optional leading `+`/`-` off `raw`, reporting whether it was `-`.
fn split_sign(raw: &str) -> (bool, &str) {
    match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw.strip_prefix('+').unwrap_or(raw)),
    }
}

/// The value under `key` when `node` is a mapping, `None` otherwise or when absent.
pub fn field<'a>(node: &'a Yaml<'a>, key: &str) -> Option<&'a Yaml<'a>> {
    node.as_mapping_get(key)
}

/// PyYAML's boolean spelling, which Ansible content uses for keywords like `ignore_errors: yes`.
/// `load` already resolves an unquoted `yes`/`no`/`on`/`off` to `Scalar::Boolean` (matching
/// PyYAML's own reading of a variable's value), so the string arm below only ever fires for a
/// *quoted* spelling: a keyword field like `gather_facts: "yes"` is still a boolean to Ansible,
/// even though the same quoting keeps a generic data value a string in `to_json`.
///
/// `0` and `1` count too. Measured on ansible-core 2.19.12: `gather_facts: 1` gathers facts and
/// `gather_facts: 0` does not, while `gather_facts: 2` refuses the load with `The value 2 could
/// not be converted to 'bool'.` Any other integer is not a boolean here either.
pub fn as_bool(node: &Yaml) -> Option<bool> {
    match node {
        Yaml::Value(Scalar::Boolean(b)) => Some(*b),
        Yaml::Value(Scalar::String(s)) => bool_from_str(s.as_ref()),
        Yaml::Value(Scalar::Integer(0)) => Some(false),
        Yaml::Value(Scalar::Integer(1)) => Some(true),
        _ => None,
    }
}

/// The spellings above, on their own, for the places that read a boolean out of plain text
/// rather than out of a YAML node: `ansible.cfg` keys and the environment variables that
/// override them, which Ansible reads with the same set of words.
pub fn bool_from_str(text: &str) -> Option<bool> {
    if PYYAML_TRUE.contains(&text) {
        return Some(true);
    }
    PYYAML_FALSE.contains(&text).then_some(false)
}

/// Converts a node to JSON. Tagged nodes are refused with a message naming the tag: vault
/// decryption and unsafe marking arrive with a later release, and silently dropping either
/// would change what a playbook does.
pub fn to_json(node: &Yaml) -> anyhow::Result<Value> {
    Ok(match node {
        Yaml::Value(Scalar::Null) => Value::Null,
        Yaml::Value(Scalar::Boolean(b)) => Value::Bool(*b),
        Yaml::Value(Scalar::Integer(i)) => Value::from(*i),
        Yaml::Value(Scalar::FloatingPoint(f)) => serde_json::Number::from_f64(f.into_inner())
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(f.to_string())),
        Yaml::Value(Scalar::String(s)) => Value::String(s.to_string()),
        Yaml::Sequence(items) => {
            Value::Array(items.iter().map(to_json).collect::<anyhow::Result<_>>()?)
        }
        Yaml::Mapping(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let k = k
                    .as_str()
                    .ok_or_else(|| anyhow!("mapping keys must be strings"))?;
                out.insert(k.to_string(), to_json(v)?);
            }
            Value::Object(out)
        }
        Yaml::Tagged(tag, _) if tag.handle == "!" && tag.suffix == "vault" => {
            bail!("vault-encrypted values (!vault) are not supported yet")
        }
        Yaml::Tagged(tag, _) if tag.handle == "!" && tag.suffix == "unsafe" => {
            bail!("!unsafe values are not supported yet")
        }
        Yaml::Tagged(tag, _) => bail!("unsupported YAML tag {}{}", tag.handle, tag.suffix),
        Yaml::Alias(_) => bail!("YAML aliases are not supported yet"),
        Yaml::Representation(..) => bail!("unresolved YAML scalar"),
        Yaml::BadValue => bail!("invalid YAML value"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalars_sequences_and_mappings_convert_to_json() {
        let docs = load("a: 1\nb: [x, 2.5, true, ~]\nc:\n  d: text\n", "t.yml").unwrap();
        let v = to_json(&docs[0]).unwrap();
        assert_eq!(
            v,
            json!({"a": 1, "b": ["x", 2.5, true, null], "c": {"d": "text"}})
        );
    }

    #[test]
    fn pyyaml_booleans_are_recognised_on_demand_only() {
        let docs = load("a: yes\nb: no\nc: maybe\n", "t.yml").unwrap();
        assert_eq!(as_bool(field(&docs[0], "a").unwrap()), Some(true));
        assert_eq!(as_bool(field(&docs[0], "b").unwrap()), Some(false));
        assert_eq!(as_bool(field(&docs[0], "c").unwrap()), None);
        // Converted as data too, matching PyYAML: unquoted `yes` reads as a variable's value
        // is a boolean, the same as Ansible's own `ignore_errors: yes`.
        assert_eq!(to_json(field(&docs[0], "a").unwrap()).unwrap(), json!(true));
    }

    #[test]
    fn a_quoted_pyyaml_boolean_spelling_stays_a_string() {
        let docs = load("flag: \"yes\"\n", "t.yml").unwrap();
        let node = field(&docs[0], "flag").unwrap();
        assert_eq!(
            as_bool(node),
            Some(true),
            "quoting doesn't stop a keyword field"
        );
        assert_eq!(
            to_json(node).unwrap(),
            json!("yes"),
            "quoting keeps a generic data value a string"
        );
    }

    /// Every spelling in the two constants, through all three readers, so the resolver,
    /// `as_bool` and `bool_from_str` cannot drift apart: they share one list by construction,
    /// and this fails if a future edit gives any of them a list of its own.
    #[test]
    fn one_list_of_boolean_spellings_serves_every_reader() {
        for (spellings, want) in [(PYYAML_TRUE, true), (PYYAML_FALSE, false)] {
            for spelling in spellings {
                assert_eq!(bool_from_str(spelling), Some(want), "{spelling}");
                let text = format!("flag: {spelling}\n");
                let docs = load(&text, "t.yml").unwrap();
                let node = field(&docs[0], "flag").unwrap();
                assert_eq!(as_bool(node), Some(want), "{spelling} as a keyword");
                assert_eq!(to_json(node).unwrap(), json!(want), "{spelling} as data");
            }
        }
        assert_eq!(bool_from_str("maybe"), None);
    }

    /// Each spelling and its type as `ansible-core 2.19.12`'s own PyYAML reads it, measured
    /// directly. `0o10` and `1e3` are the two saphyr resolves as numbers under YAML 1.2 and
    /// PyYAML leaves alone; the exponent forms show that PyYAML wants both a `.` in the
    /// mantissa and a sign on the exponent.
    #[test]
    fn integer_and_float_spellings_are_measured_against_the_reference() {
        let cases: &[(&str, Value)] = &[
            ("010", json!(8)),
            ("\"010\"", json!("010")),
            ("0_755", json!(493)),
            ("08", json!("08")),
            ("0o10", json!("0o10")),
            ("0b1010", json!(10)),
            ("0b_1010", json!(10)),
            ("0x1F", json!(31)),
            ("0x_1F", json!(31)),
            ("1_000", json!(1000)),
            ("1000", json!(1000)),
            ("+12", json!(12)),
            ("-07", json!(-7)),
            ("+0b11", json!(3)),
            ("0", json!(0)),
            ("00", json!(0)),
            ("0_0", json!(0)),
            ("1e3", json!("1e3")),
            ("1e+3", json!("1e+3")),
            ("1.0e3", json!("1.0e3")),
            ("1.0e+3", json!(1000.0)),
            ("2.5", json!(2.5)),
            (".5", json!(0.5)),
            ("1_0.5", json!(10.5)),
            ("1e", json!("1e")),
            ("0xg", json!("0xg")),
        ];
        for (raw, want) in cases {
            let text = format!("x: {raw}\n");
            let docs = load(&text, "t.yml").unwrap();
            let got = to_json(field(&docs[0], "x").unwrap()).unwrap();
            assert_eq!(&got, want, "{raw}");
        }
    }

    #[test]
    fn vault_and_unsafe_tags_are_detected_and_refused_for_now() {
        let docs = load(
            "secret: !vault |\n  $ANSIBLE_VAULT;1.1;AES256\n  3132\n",
            "t.yml",
        )
        .unwrap();
        let err = to_json(&docs[0]).unwrap_err();
        assert!(format!("{err:#}").contains("vault"), "{err:#}");
        let docs = load("raw: !unsafe '{{ not a template }}'\n", "t.yml").unwrap();
        let err = to_json(&docs[0]).unwrap_err();
        assert!(format!("{err:#}").contains("unsafe"), "{err:#}");
    }

    #[test]
    fn syntax_errors_name_the_source() {
        let err = load("a: [1, 2\n", "broken.yml").unwrap_err();
        assert!(format!("{err:#}").contains("broken.yml"));
    }

    #[test]
    fn missing_fields_are_none_not_panics() {
        let docs = load("a: 1\n", "t.yml").unwrap();
        assert!(field(&docs[0], "zzz").is_none());
        assert!(field(&docs[0], "a").is_some());
    }
}
