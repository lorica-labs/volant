// SPDX-License-Identifier: GPL-3.0-or-later
//! What every native shares with the reference's module machinery.
// Module-wide: which of these items rustc reports as dead differs between the MSRV and the
// pinned toolchain, so one expectation covers the file.
#![cfg_attr(not(test), expect(dead_code, reason = "used by the first native"))]

use serde_json::{Map, Value};

/// One entry of a module's `argument_spec`, copied from ansible-core 2.19.12.
pub struct ArgSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// The reference's default, `Value::Null` where it has none.
    pub default: fn() -> Value,
}

/// The `invocation` a native returns: `{"module_args": ...}` holding every argument of `spec`,
/// the way `AnsibleModule` fills its validated parameters.
///
/// An alias sets its canonical argument and stays in the map under its own name too; when
/// several spellings are given, the last alias in the spec's list wins, over the canonical name
/// as well. An argument nobody gave gets its default, `null` included. Measured on
/// ansible-core 2.19.12: `stat` given `dest` returns both `dest` and `path` in `module_args`.
///
/// Arguments outside the spec are left as given: refusing them is the native's decision, made
/// before it answers at all.
pub fn invocation(spec: &[ArgSpec], args: &Map<String, Value>) -> Value {
    let mut module_args = args.clone();
    for arg in spec {
        for alias in arg.aliases {
            if let Some(value) = args.get(*alias) {
                module_args.insert(arg.name.to_string(), value.clone());
            }
        }
        if !module_args.contains_key(arg.name) {
            module_args.insert(arg.name.to_string(), (arg.default)());
        }
    }
    let mut invocation = Map::new();
    invocation.insert("module_args".into(), Value::Object(module_args));
    Value::Object(invocation)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// `stat`'s shape against the reference: `dest` given, `path` filled from it, both kept, and
    /// every other argument at its default, `null` ones included.
    ///
    /// What would make this red: the alias dropped from the map, or the canonical name left
    /// unset, both of which the golden sees as a different `invocation`; a `null` default left
    /// out, which the reference prints; or the canonical name winning over an alias given with
    /// it, which is the reverse of what `_handle_aliases` does.
    #[test]
    fn the_invocation_resolves_aliases_and_fills_defaults_like_the_reference() {
        const SPEC: &[ArgSpec] = &[
            ArgSpec {
                name: "path",
                aliases: &["dest", "name"],
                default: || Value::Null,
            },
            ArgSpec {
                name: "follow",
                aliases: &[],
                default: || Value::Bool(false),
            },
            ArgSpec {
                name: "checksum_algorithm",
                aliases: &["checksum"],
                default: || json!("sha1"),
            },
            ArgSpec {
                name: "get_mime",
                aliases: &[],
                default: || Value::Null,
            },
        ];
        let args = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(
            invocation(SPEC, &args(json!({"dest": "/etc/hostname"}))),
            json!({"module_args": {
                "dest": "/etc/hostname",
                "path": "/etc/hostname",
                "follow": false,
                "checksum_algorithm": "sha1",
                "get_mime": null,
            }})
        );
        assert_eq!(
            invocation(
                SPEC,
                &args(json!({"path": "/a", "dest": "/b", "name": "/c"}))
            )["module_args"]["path"],
            "/c",
            "the last alias given wins, over the canonical name too"
        );
    }
}
