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

/// Re-resolves a scalar the way ansible-core's PyYAML (YAML 1.1) would, for the spellings
/// measured to diverge from saphyr's YAML 1.2 core schema: `yes`/`no`/`on`/`off` (plain,
/// capitalised or upper-case) as booleans, leading-zero or underscore-grouped plain integers,
/// and a leading zero followed by a digit outside `0`-`7` (e.g. `08`), which PyYAML's own
/// integer resolver rejects and leaves a string even though it looks numeric. `raw` is the
/// node's exact source text; quoting it, or writing it as a block scalar, makes `raw` start
/// with something other than the bare word or digits, which leaves `scalar` untouched here too
/// — matching PyYAML, which also only resolves *plain* scalars this way, so a deliberately
/// quoted `"yes"` stays the string a playbook wrote.
fn pyyaml_scalar<'a>(scalar: Scalar<'a>, raw: &'a str) -> Scalar<'a> {
    match raw {
        "yes" | "Yes" | "YES" | "on" | "On" | "ON" => return Scalar::Boolean(true),
        "no" | "No" | "NO" | "off" | "Off" | "OFF" => return Scalar::Boolean(false),
        _ => {}
    }
    if let Some(i) = pyyaml_octal(raw) {
        return Scalar::Integer(i);
    }
    if let Some(i) = pyyaml_underscored_decimal(raw) {
        return Scalar::Integer(i);
    }
    if pyyaml_rejects_as_int(raw) {
        return Scalar::String(raw.into());
    }
    scalar
}

/// PyYAML's octal spelling: an optional sign, a leading `0`, then one or more further octal
/// digits or `_` separators, e.g. `010` (8) or `0_755` (493).
fn pyyaml_octal(raw: &str) -> Option<i64> {
    let (negative, rest) = split_sign(raw);
    let digits = rest.strip_prefix('0')?;
    if digits.is_empty() || !digits.bytes().all(|b| matches!(b, b'0'..=b'7' | b'_')) {
        return None;
    }
    let value = i64::from_str_radix(&digits.replace('_', ""), 8).ok()?;
    Some(if negative { -value } else { value })
}

/// A leading zero followed only by more digits and/or `_`, but with at least one `8` or `9` in
/// them (so [`pyyaml_octal`] already refused it): not valid octal, and PyYAML's decimal branch
/// only ever matches a bare `0` or a run starting `1`-`9`, so it doesn't match that either.
/// PyYAML leaves it an unresolved string; saphyr's own decimal parser doesn't share PyYAML's
/// no-leading-zero rule and would otherwise read it as a number, e.g. `08` as `8`. Measured
/// directly against ansible-core.
fn pyyaml_rejects_as_int(raw: &str) -> bool {
    let (_, rest) = split_sign(raw);
    match rest.strip_prefix('0') {
        Some(digits) if !digits.is_empty() => {
            digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
        }
        _ => false,
    }
}

/// PyYAML's underscore-grouped decimal spelling: an optional sign, a first digit `1`-`9`, then
/// further digits or `_` separators, e.g. `1_000` (1000). Plain digits with no underscore
/// already resolve identically on both sides, so this only fires when `raw` contains one.
fn pyyaml_underscored_decimal(raw: &str) -> Option<i64> {
    if !raw.contains('_') {
        return None;
    }
    let (negative, rest) = split_sign(raw);
    if !matches!(rest.as_bytes().first(), Some(b'1'..=b'9')) {
        return None;
    }
    if !rest.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
        return None;
    }
    let value: i64 = rest.replace('_', "").parse().ok()?;
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
pub fn as_bool(node: &Yaml) -> Option<bool> {
    match node {
        Yaml::Value(Scalar::Boolean(b)) => Some(*b),
        Yaml::Value(Scalar::String(s)) => bool_from_str(s.as_ref()),
        _ => None,
    }
}

/// The spellings above, on their own, for the places that read a boolean out of plain text
/// rather than out of a YAML node: `ansible.cfg` keys and the environment variables that
/// override them, which Ansible reads with the same set of words.
pub fn bool_from_str(text: &str) -> Option<bool> {
    match text {
        "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Some(true),
        "false" | "False" | "FALSE" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Some(false),
        _ => None,
    }
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

    #[test]
    fn pyyaml_octal_and_underscored_integers_are_measured_against_the_reference() {
        let docs = load(
            "octal: 010\nquoted_octal: \"010\"\nbad_octal: 08\ngrouped: 1_000\nplain: 1000\n",
            "t.yml",
        )
        .unwrap();
        assert_eq!(
            to_json(field(&docs[0], "octal").unwrap()).unwrap(),
            json!(8)
        );
        assert_eq!(
            to_json(field(&docs[0], "quoted_octal").unwrap()).unwrap(),
            json!("010")
        );
        assert_eq!(
            to_json(field(&docs[0], "bad_octal").unwrap()).unwrap(),
            json!("08"),
            "08 has no valid octal digit after the leading zero"
        );
        assert_eq!(
            to_json(field(&docs[0], "grouped").unwrap()).unwrap(),
            json!(1000)
        );
        assert_eq!(
            to_json(field(&docs[0], "plain").unwrap()).unwrap(),
            json!(1000)
        );
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
