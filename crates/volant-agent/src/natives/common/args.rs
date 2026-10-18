// SPDX-License-Identifier: GPL-3.0-or-later
//! The arguments of a native: the spec each one declares, the `invocation` it returns, and the
//! typed reads of its parameters.

use serde_json::{Map, Value};

/// One entry of a module's `argument_spec`, copied from ansible-core 2.19.12.
pub struct ArgSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// The reference's default, `Value::Null` where it has none.
    pub default: fn() -> Value,
}

/// An argument without a default.
pub const fn null(name: &'static str) -> ArgSpec {
    ArgSpec {
        name,
        aliases: &[],
        default: || Value::Null,
    }
}

/// A `type='bool'` argument and its default.
pub const fn flag(name: &'static str, default: bool) -> ArgSpec {
    ArgSpec {
        name,
        aliases: &[],
        default: if default {
            || Value::Bool(true)
        } else {
            || Value::Bool(false)
        },
    }
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
    let mut invocation = Map::new();
    invocation.insert("module_args".into(), Value::Object(module_args(spec, args)));
    Value::Object(invocation)
}

/// The validated parameters `invocation` wraps, for a native that reads them, or rewrites one
/// as the reference's module does to its own `params` before it answers.
pub fn module_args(spec: &[ArgSpec], args: &Map<String, Value>) -> Map<String, Value> {
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
    module_args
}

/// Hands the task back unless every argument is one of `spec`'s, given under one name.
///
/// An unknown argument fails the reference's validation, and an option given under two of its
/// names adds a warning (`Both option path and its alias dest are set.`): the Python module says
/// either better than a native would.
pub fn check_names(spec: &[ArgSpec], args: &Map<String, Value>) -> Result<(), String> {
    for key in args.keys() {
        if !spec
            .iter()
            .any(|arg| arg.name == key || arg.aliases.contains(&key.as_str()))
        {
            return Err(format!("argument {key} is unknown here"));
        }
    }
    for arg in spec {
        let names = std::iter::once(arg.name).chain(arg.aliases.iter().copied());
        if names.filter(|name| args.contains_key(*name)).count() > 1 {
            return Err(format!("{} is given under more than one name", arg.name));
        }
    }
    Ok(())
}

/// A `type='bool'` parameter, taken only as a JSON boolean. The reference also converts `"yes"`,
/// `1` and the like, and shows the converted value in `invocation`; the Python module does that.
pub fn bool_param(params: &Map<String, Value>, name: &str) -> Result<bool, String> {
    params
        .get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("{name} is not a boolean"))
}

/// A `type='str'` parameter: `None` when null, taken only as a JSON string otherwise (the
/// reference converts a number and warns about it).
pub fn str_param<'a>(
    params: &'a Map<String, Value>,
    name: &str,
) -> Result<Option<&'a str>, String> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("{name} is not a string")),
    }
}

/// A required `type='path'` parameter. The reference expands `~` and `$VAR` in it and resolves a
/// relative one against the module's own working directory; a native takes only an absolute path
/// that none of that changes.
pub fn path_param<'a>(params: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    match str_param(params, name)? {
        Some(path) if path.starts_with('/') && !path.contains(['$', '\0']) => Ok(path),
        _ => Err(format!("{name} is not a plain absolute path")),
    }
}
