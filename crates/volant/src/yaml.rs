// SPDX-License-Identifier: GPL-3.0-or-later
//! YAML loading shared by playbooks, variable files and inventories, with the PyYAML habits
//! Ansible content relies on (`yes`/`no` booleans) and the tags Ansible adds (`!vault`,
//! `!unsafe`).

use anyhow::{Context, anyhow, bail};
use saphyr::{LoadableYamlNode, Scalar, Yaml};
use serde_json::{Map, Value};

/// Loads every document of `text`. Errors name `source`.
pub fn load<'a>(text: &'a str, source: &str) -> anyhow::Result<Vec<Yaml<'a>>> {
    Yaml::load_from_str(text).with_context(|| format!("{source}: invalid YAML"))
}

/// The value under `key` when `node` is a mapping, `None` otherwise or when absent.
pub fn field<'a>(node: &'a Yaml<'a>, key: &str) -> Option<&'a Yaml<'a>> {
    node.as_mapping_get(key)
}

/// PyYAML's boolean spelling, which Ansible content uses for keywords like `ignore_errors: yes`.
/// Applied only where a boolean is expected: as data, `yes` stays a string.
pub fn as_bool(node: &Yaml) -> Option<bool> {
    match node {
        Yaml::Value(Scalar::Boolean(b)) => Some(*b),
        Yaml::Value(Scalar::String(s)) => match s.as_ref() {
            "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Some(true),
            "false" | "False" | "FALSE" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Some(false),
            _ => None,
        },
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
        let docs = load("on: yes\noff: no\nword: maybe\n", "t.yml").unwrap();
        assert_eq!(as_bool(field(&docs[0], "on").unwrap()), Some(true));
        assert_eq!(as_bool(field(&docs[0], "off").unwrap()), Some(false));
        assert_eq!(as_bool(field(&docs[0], "word").unwrap()), None);
        // Converted as data, `yes` stays the string Ansible would also keep in a variable.
        assert_eq!(
            to_json(field(&docs[0], "on").unwrap()).unwrap(),
            json!("yes")
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
