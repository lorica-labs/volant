// SPDX-License-Identifier: GPL-3.0-or-later

//! The argument check a role with a `meta/argument_specs.yml` gets in front of it: ansible-core
//! 2.19.12's `ArgumentSpecValidator` (`module_utils/common/arg_spec.py` and `parameters.py`),
//! pass for pass and sentence for sentence.

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use crate::executor::prepare::{Item, display};
use crate::template::truthy;

use super::families;
use super::types::{convert, dict_from_str, native_type, py_eq, tagged_name};

type Params = Map<String, Value>;

/// An argument the spec does not take: its dotted name, the options the spec at that level has
/// and their aliases.
type Unsupported = (String, Vec<String>, Vec<String>);

/// Runs the role's check, as the reference's `validate_argument_spec` action does.
///
/// Where the arguments come from is measured, not assumed: a value the role entry never named
/// is still found when the host has a variable of that name (a `-e` on the command line passes
/// the check), while a name that is **not** in the spec is refused only when the entry wrote it,
/// because every host carries hundreds of variables the spec has never heard of.
///
/// A converted value is not written back, which is what the reference does too - measured, a
/// role reading `count: "3"` against a `type: int` sees the string it was given.
pub(in crate::executor::run) fn validate_argument_spec(item: &Item) -> TaskResult {
    let spec = match item.args.get("argument_spec") {
        Some(Value::Object(spec)) => spec.clone(),
        _ => Map::new(),
    };
    // `combine_vars(args_from_vars, provided_arguments)`: what the host has under a name the
    // spec knows, then the entry's own on top.
    let mut params: Params = spec
        .keys()
        .filter_map(|name| Some((name.clone(), item.vars.get(name.as_str())?.clone())))
        .collect();
    if let Some(Value::Object(provided)) = item.args.get("provided_arguments") {
        params.extend(provided.clone());
    }
    let errors = validate(&spec, &mut params);
    let context = item
        .args
        .get("validate_args_context")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let mut r = Map::new();
    r.insert("changed".into(), json!(false));
    if errors.is_empty() {
        r.insert("msg".into(), json!("The arg spec validation passed"));
        r.insert("validate_args_context".into(), context);
        return TaskResult(r);
    }
    r.insert(
        "msg".into(),
        json!(format!(
            "Validation of arguments failed:\n{}",
            errors.join("\n")
        )),
    );
    r.insert("argument_errors".into(), json!(errors));
    r.insert("argument_spec_data".into(), Value::Object(spec));
    r.insert("validate_args_context".into(), context);
    r.insert("failed".into(), json!(true));
    TaskResult(r)
}

/// `ArgumentSpecValidator.validate`: every error, in the order the reference finds them.
/// No-log values and deprecations are not collected: the action reports neither.
pub(super) fn validate(spec: &Params, params: &mut Params) -> Vec<String> {
    let mut errors = Vec::new();
    let mut unsupported = Vec::new();
    let aliases = handle_aliases(spec, params).unwrap_or_else(|e| {
        errors.push(e);
        Vec::new()
    });
    errors.extend(sub_dicts(spec, params).err());
    unsupported_parameters(spec, params, &aliases, &[], &mut unsupported);
    set_defaults(spec, params, false);
    errors.extend(required_arguments(spec, params, &[]).err());
    argument_types(spec, params, &[], &mut errors);
    argument_values(spec, params, &[], &mut errors);
    set_defaults(spec, params, true);
    sub_spec(spec, params, &mut Vec::new(), &mut errors, &mut unsupported);
    unsupported.sort();
    unsupported.dedup_by(|a, b| a.0 == b.0);
    // The supported names are those of the last unsupported argument the reference iterates
    // over, a set whose order changes between runs: the last by name here.
    if let Some((_, supported, aliases)) = unsupported.last() {
        let names: Vec<&str> = unsupported.iter().map(|u| u.0.as_str()).collect();
        let mut supported = supported.clone();
        supported.sort();
        let mut list = supported.join(", ");
        if !aliases.is_empty() {
            list = format!("{list} ({})", aliases.join(", "));
        }
        errors.push(format!(
            "{}. Supported parameters include: {list}.",
            names.join(", ")
        ));
    }
    errors
}

/// What `msg` says inside a sub-option, as `check_*` and the choice check write it.
pub(super) fn found_in(msg: String, context: &[String]) -> String {
    if context.is_empty() {
        msg
    } else {
        format!("{msg} found in {}", context.join(" -> "))
    }
}

fn required(option: &Value) -> bool {
    option.get("required").is_some_and(truthy)
}

fn default(option: &Value) -> Value {
    option.get("default").cloned().unwrap_or(Value::Null)
}

/// Whether the option holds sub-options: a `dict`, or a `list` of them.
fn holds_dicts(option: &Value) -> bool {
    let wanted = option.get("type").and_then(Value::as_str);
    wanted == Some("dict")
        || (wanted == Some("list")
            && option.get("elements").and_then(Value::as_str) == Some("dict"))
}

/// `_handle_aliases`: each alias's value copied under its option, the aliases sorted.
fn handle_aliases(spec: &Params, params: &mut Params) -> Result<Vec<String>, String> {
    let mut aliases = Vec::new();
    for (name, option) in spec {
        if !default(option).is_null() && required(option) {
            return Err(format!(
                "internal error: required and default are mutually exclusive for {name}"
            ));
        }
        match option.get("aliases") {
            None | Some(Value::Null) => {}
            Some(Value::Array(list)) => {
                for alias in list.iter().filter_map(Value::as_str) {
                    aliases.push(alias.to_string());
                    if let Some(value) = params.get(alias).cloned() {
                        params.insert(name.clone(), value);
                    }
                }
            }
            Some(_) => return Err("internal error: aliases must be a list or tuple".into()),
        }
    }
    aliases.sort();
    aliases.dedup();
    Ok(aliases)
}

/// The sub-option walk of `_list_no_log_values`, which reads a string given for a `dict` as one
/// and stops at the first value that is neither.
fn sub_dicts(spec: &Params, params: &Params) -> Result<(), String> {
    for (name, option) in spec {
        let (Some(Value::Object(sub_spec)), Some(value)) =
            (option.get("options"), params.get(name))
        else {
            continue;
        };
        if value.is_null() || !holds_dicts(option) {
            continue;
        }
        let items = match value {
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        for item in items {
            let item = match item {
                Value::String(text) => dict_from_str(&text)?,
                other => other,
            };
            let Value::Object(sub) = &item else {
                return Err(format!(
                    "Value '{}' in the sub parameter field '{name}' must be a {}, not '{}'",
                    display(&item),
                    option
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    tagged_name(&item)
                ));
            };
            sub_dicts(sub_spec, sub)?;
        }
    }
    Ok(())
}

fn unsupported_parameters(
    spec: &Params,
    params: &Params,
    aliases: &[String],
    context: &[String],
    out: &mut Vec<Unsupported>,
) {
    for key in params.keys() {
        if spec.contains_key(key) || aliases.contains(key) {
            continue;
        }
        let mut path = context.to_vec();
        path.push(key.clone());
        let supported = spec
            .keys()
            .filter(|name| !aliases.contains(name))
            .cloned()
            .collect();
        out.push((path.join("."), supported, aliases.to_vec()));
    }
}

/// `_set_defaults`: the first pass sets only the defaults that are not `None`, the second
/// gives every option still missing its default.
fn set_defaults(spec: &Params, params: &mut Params, all: bool) {
    for (name, option) in spec {
        let default = default(option);
        if !params.contains_key(name) && (all || !default.is_null()) {
            params.insert(name.clone(), default);
        }
    }
}

/// Only an option nobody set is missing: one set to `~` is there, and its type says whether
/// `None` will do.
fn required_arguments(spec: &Params, params: &Params, context: &[String]) -> Result<(), String> {
    let mut missing: Vec<&str> = spec
        .iter()
        .filter(|(name, option)| required(option) && !params.contains_key(*name))
        .map(|(name, _)| name.as_str())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort_unstable();
    Err(found_in(
        format!("missing required arguments: {}", missing.join(", ")),
        context,
    ))
}

/// `_validate_argument_types`: each value converted in place, so the later checks see what
/// the type made of it.
fn argument_types(
    spec: &Params,
    params: &mut Params,
    context: &[String],
    errors: &mut Vec<String>,
) {
    let quoted = if context.is_empty() {
        String::new()
    } else {
        format!(" found in '{}'.", context.join(" -> "))
    };
    for (name, option) in spec {
        let Some(value) = params.get(name).cloned() else {
            continue;
        };
        // `None` for an option neither required nor defaulted is left alone.
        if value.is_null() && !required(option) && default(option).is_null() {
            continue;
        }
        let wanted = type_name(option.get("type"));
        let converted = match convert(&value, &wanted) {
            Ok(converted) => converted,
            Err(why) => {
                errors.push(format!(
                    "argument '{name}' is of type {}{quoted} and we were unable to convert to {wanted}: {why}",
                    native_type(&value)
                ));
                continue;
            }
        };
        let Some(elements) = option.get("elements").filter(|e| truthy(e)) else {
            params.insert(name.clone(), converted);
            continue;
        };
        let Value::Array(items) = &converted else {
            errors.push(format!(
                "Invalid type {wanted} for option '{}'{quoted}, elements value check is supported only with 'list' type",
                display(&converted)
            ));
            params.insert(name.clone(), converted);
            continue;
        };
        let element_type = type_name(Some(elements));
        let mut kept = Vec::new();
        for item in items {
            match convert(item, &element_type) {
                Ok(item) => kept.push(item),
                Err(why) => errors.push(format!(
                    "Elements value for option '{name}'{} is of type {} and we were unable to convert to {element_type}: {why}",
                    if context.is_empty() {
                        String::new()
                    } else {
                        format!(" found in '{}'", context.join(" -> "))
                    },
                    native_type(item)
                )),
            }
        }
        params.insert(name.clone(), Value::Array(kept));
    }
}

/// The name of the type an option asks for, `str` when it names none.
fn type_name(wanted: Option<&Value>) -> String {
    match wanted {
        None | Some(Value::Null) => "str".into(),
        Some(other) => display(other),
    }
}

/// `_validate_argument_values`: `choices`, where a list must hold only choices, and a `"True"`
/// or `"False"` YAML made of a boolean reads as the one boolean choice it can only mean.
fn argument_values(
    spec: &Params,
    params: &mut Params,
    context: &[String],
    errors: &mut Vec<String>,
) {
    for (name, option) in spec {
        let choices = match option.get("choices") {
            None | Some(Value::Null) => continue,
            Some(Value::Array(choices)) => choices,
            Some(other) => {
                errors.push(found_in(
                    format!(
                        "internal error: choices for argument {name} are not iterable: {}",
                        display(other)
                    ),
                    context,
                ));
                continue;
            }
        };
        let Some(value) = params.get(name) else {
            continue;
        };
        let listed = choices.iter().map(display).collect::<Vec<_>>().join(", ");
        let is_choice = |v: &Value| choices.iter().any(|c| py_eq(c, v));
        if let Value::Array(items) = value {
            let off: Vec<String> = items
                .iter()
                .filter(|i| !is_choice(i))
                .map(display)
                .collect();
            if !off.is_empty() {
                errors.push(found_in(
                    format!(
                        "value of {name} must be one or more of: {listed}. Got no match for: {}",
                        off.join(", ")
                    ),
                    context,
                ));
            }
            continue;
        }
        if is_choice(value) {
            continue;
        }
        if let Some(choice) = boolean_choice(value, choices) {
            params.insert(name.clone(), choice);
            continue;
        }
        errors.push(found_in(
            format!(
                "value of {name} must be one of: {listed}, got: {}",
                display(value)
            ),
            context,
        ));
    }
}

/// The one choice in `BOOLEANS_TRUE` or `BOOLEANS_FALSE` a `"True"` or `"False"` stands for.
fn boolean_choice(value: &Value, choices: &[Value]) -> Option<Value> {
    let set = match value.as_str()? {
        "True" => json!(["y", "yes", "on", "1", "true", "t", 1, true]),
        "False" => json!(["n", "no", "off", "0", "false", "f", 0, false]),
        _ => return None,
    };
    let mut overlap: Vec<&Value> = Vec::new();
    for choice in choices {
        let is_boolean = set
            .as_array()
            .is_some_and(|s| s.iter().any(|b| py_eq(b, choice)));
        if is_boolean && !overlap.iter().any(|o| py_eq(o, choice)) {
            overlap.push(choice);
        }
    }
    match overlap.as_slice() {
        [one] => Some((*one).clone()),
        _ => None,
    }
}

/// `_validate_sub_spec`: each `dict` option with `options`, and each element of a list of them,
/// checked as a spec of its own, deeper levels after.
fn sub_spec(
    spec: &Params,
    params: &mut Params,
    context: &mut Vec<String>,
    errors: &mut Vec<String>,
    unsupported: &mut Vec<Unsupported>,
) {
    for (name, option) in spec {
        if !holds_dicts(option) {
            continue;
        }
        let Some(Value::Object(sub_spec_of)) = option.get("options") else {
            continue;
        };
        if option.get("apply_defaults").is_some_and(truthy) {
            if params.get(name).is_none_or(Value::is_null) {
                params.insert(name.clone(), json!({}));
            }
        } else if params.get(name).is_none_or(Value::is_null) {
            continue;
        }
        let Some(value) = params.get_mut(name) else {
            continue;
        };
        context.push(name.clone());
        let elements: Vec<&mut Value> = match value {
            Value::Array(items) => items.iter_mut().collect(),
            other => vec![other],
        };
        for element in elements {
            let Value::Object(sub) = element else {
                errors.push(format!(
                    "value of '{name}' must be of type dict or list of dicts"
                ));
                continue;
            };
            let aliases = handle_aliases(sub_spec_of, sub).unwrap_or_else(|e| {
                errors.push(e);
                Vec::new()
            });
            errors.extend(sub_dicts(sub_spec_of, sub).err());
            unsupported_parameters(sub_spec_of, sub, &aliases, context, unsupported);
            errors.extend(
                families::mutually_exclusive(option.get("mutually_exclusive"), sub, context).err(),
            );
            set_defaults(sub_spec_of, sub, false);
            errors.extend(required_arguments(sub_spec_of, sub, context).err());
            argument_types(sub_spec_of, sub, context, errors);
            argument_values(sub_spec_of, sub, context, errors);
            errors.extend(families::required(option, sub, context));
            set_defaults(sub_spec_of, sub, true);
            sub_spec(sub_spec_of, sub, context, errors, unsupported);
        }
        context.pop();
    }
}
