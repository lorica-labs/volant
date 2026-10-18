// SPDX-License-Identifier: GPL-3.0-or-later
//! The arguments, read the way `AnsibleModule` reads them, inside the fast path.

use serde_json::{Map, Value};

use super::super::common::invocation;
use super::spec::{SPEC, TAKEN, Take};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    Present,
    Absent,
}

/// The arguments, as `AnsibleModule` would have validated them, inside the fast path.
#[derive(Debug)]
pub(super) struct Request {
    pub(super) state: State,
    pub(super) packages: Vec<String>,
    pub(super) update_cache: bool,
    pub(super) cache_valid_time: i64,
    pub(super) module_args: Map<String, Value>,
}

/// The arguments checked against the spec and converted the way `AnsibleModule` converts
/// them. Anything the module would refuse, or would act on outside the fast path, is an `Err`:
/// the module then gives its own answer, failure messages included.
pub(super) fn request(args: &Map<String, Value>) -> Result<Request, String> {
    for key in args.keys() {
        let arg = SPEC
            .iter()
            .find(|arg| arg.name == key || arg.aliases.contains(&key.as_str()))
            .ok_or_else(|| format!("argument '{key}' is outside the native apt"))?;
        let given = std::iter::once(arg.name)
            .chain(arg.aliases.iter().copied())
            .filter(|name| args.contains_key(*name))
            .count();
        if given > 1 {
            return Err(format!("{} is given under more than one name", arg.name));
        }
    }
    // `deb` and `upgrade` are mutually exclusive with `package`, and counted by presence.
    for key in ["deb", "upgrade"] {
        if args.contains_key(key) {
            return Err(format!("{key} is outside the native apt"));
        }
    }
    let value = |name: &str| -> Option<&Value> {
        let arg = SPEC.iter().find(|arg| arg.name == name)?;
        std::iter::once(arg.name)
            .chain(arg.aliases.iter().copied())
            .find_map(|name| args.get(name))
    };

    let mut module_args = match invocation(SPEC, args) {
        Value::Object(mut invocation) => match invocation.remove("module_args") {
            Some(Value::Object(module_args)) => module_args,
            _ => Map::new(),
        },
        _ => Map::new(),
    };

    let state = match value("state") {
        None => State::Present,
        Some(Value::String(state)) if state == "present" => State::Present,
        Some(Value::String(state)) if state == "absent" => State::Absent,
        Some(_) => return Err("a state other than present or absent".into()),
    };
    match value("default_release") {
        None | Some(Value::Null) => {}
        Some(Value::String(release)) if release.is_empty() => {}
        Some(_) => return Err("default_release is outside the native apt".into()),
    }

    for (name, take) in TAKEN {
        let Some(given) = value(name) else { continue };
        let converted = match (take, given) {
            (Take::OptBool | Take::OptInt, Value::Null) => Value::Null,
            (Take::Bool | Take::OptBool | Take::False, given) => {
                let b = to_bool(given).ok_or_else(|| format!("{name} is not a boolean"))?;
                if matches!(take, Take::False) && b {
                    return Err(format!("{name} is outside the native apt"));
                }
                b.into()
            }
            (Take::Int | Take::OptInt, given) => to_int(given)
                .ok_or_else(|| format!("{name} is not an integer"))?
                .into(),
            (Take::Str, Value::String(text)) => text.clone().into(),
            (Take::Str, _) => return Err(format!("{name} is not a string")),
        };
        module_args.insert((*name).to_string(), converted);
    }
    let update_cache = module_args["update_cache"].as_bool().unwrap_or(false);
    let cache_valid_time = match value("cache_valid_time") {
        None => 0,
        Some(given) => to_int(given).ok_or("cache_valid_time is not an integer")?,
    };
    // Past a billion seconds `datetime` arithmetic can overflow in the module.
    if !(0..=1_000_000_000).contains(&cache_valid_time) {
        return Err("cache_valid_time is out of the native range".into());
    }
    module_args.insert("cache_valid_time".into(), cache_valid_time.into());

    let packages = match value("package") {
        // `required_one_of` always holds: `autoremove` gets its default before it is checked.
        None => Vec::new(),
        // `type='list', elements='str'`: a string is split on commas.
        Some(Value::String(text)) => text.split(',').map(str::to_string).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or("a package name is not a string")?,
        Some(_) => return Err("package is not a list of names".into()),
    };
    if value("package").is_some() {
        if packages.is_empty() {
            return Err("the package list is empty".into());
        }
        if !packages.iter().all(|name| is_exact_name(name)) {
            return Err("a package is not given by its exact name".into());
        }
        module_args.insert(
            "package".into(),
            Value::Array(packages.iter().cloned().map(Value::from).collect()),
        );
    }
    Ok(Request {
        state,
        packages,
        update_cache,
        cache_valid_time,
        module_args,
    })
}

/// A Debian package name and nothing else: no version (`=`, `>=`), no pattern (`*?[]!`), no
/// architecture (`:`), no space for the module to strip.
fn is_exact_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 2
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.')
        })
}

/// `check_type_bool`: a bool, or a string or number among the spellings it takes.
fn to_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => {
            let n = n.as_f64()?;
            (n == 1.0 || n == 0.0).then_some(n == 1.0)
        }
        Value::String(text) => match text.to_lowercase().trim() {
            "y" | "yes" | "on" | "1" | "true" | "t" => Some(true),
            "n" | "no" | "off" | "0" | "false" | "f" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `check_type_int`: an integer, or a string `int()` reads in plain ASCII digits.
fn to_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(text) => {
            let text = text.trim();
            let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            text.parse().ok()
        }
        _ => None,
    }
}
