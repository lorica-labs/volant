// SPDX-License-Identifier: GPL-3.0-or-later
//! `ansible._protomatter`: the internal filters and tests ansible-core's own integration targets
//! read tags with. Not a public interface there, and not one here.

use std::sync::Arc;

use minijinja::value::{Kwargs, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State, Value};

use super::tagged::{tag, tags_of};
use crate::tags::{TagSet, TagSetId};

const NAMESPACE: &str = "ansible._protomatter";

/// The types `unmask` knows: `_type_transform_mapping` of ansible-core 2.19.12.
const MASKED: [&str; 7] = [
    "CapturedErrorSummary",
    "DeprecationSummary",
    "EncryptedString",
    "ErrorSummary",
    "PluginInfo",
    "PluginType",
    "WarningSummary",
];

/// The tag names `tagged_with` knows.
const TAG_NAMES: [&str; 5] = [
    "Deprecated",
    "Origin",
    "SourceWasEncrypted",
    "TrustedAsTemplate",
    "VaultedValue",
];

/// Registers `f` as `ansible._protomatter.<name>`, answering undefined for an undefined value:
/// every one of them does, as the target checks.
fn filter(
    env: &mut Environment<'static>,
    name: &str,
    f: impl Fn(&State, Value, Kwargs) -> Result<Value, Error> + Send + Sync + 'static,
) {
    env.add_filter(
        format!("{NAMESPACE}.{name}"),
        move |state: &State, value: Value, args: Kwargs| {
            if value.is_undefined() {
                return Ok(Value::UNDEFINED);
            }
            f(state, value, args)
        },
    );
}

pub(super) fn register(env: &mut Environment<'static>) {
    filter(env, "origin", |state, value, _| {
        let set = crate::tags::get(tags_of(state, &value));
        Ok(set.origin.map_or(Value::from(()), |o| {
            Value::from(crate::tags::origin_text(o))
        }))
    });
    filter(env, "tag_names", |state, value, _| {
        Ok(Value::from(crate::tags::names(tags_of(state, &value))))
    });
    filter(env, "apply_trust", |state, value, _| {
        let Some(text) = value.as_str() else {
            return Ok(value);
        };
        let mut set = crate::tags::get(tags_of(state, &value));
        set.trusted = true;
        let copy = Value::from(Arc::<str>::from(text));
        tag(state, &copy, crate::tags::intern(set));
        Ok(copy)
    });
    filter(env, "dump_object", |_, value, _| Ok(value));
    filter(env, "finalize", |_, value, _| Ok(value));
    filter(env, "python_literal_eval", |_, value, args| {
        let ignore: bool = args.get::<Option<bool>>("ignore_errors")?.unwrap_or(false);
        args.assert_all_used()?;
        let Some(text) = value.as_str() else {
            if ignore {
                return Ok(value);
            }
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                "The `value` to eval must be a string.",
            ));
        };
        match crate::inventory::literal_eval(text) {
            Some(json) => Ok(super::bigint::to_template(&json)),
            None if ignore => Ok(value),
            None => Err(Error::new(
                ErrorKind::InvalidOperation,
                format!("malformed node or string: {text:?}"),
            )),
        }
    });
    filter(env, "true_type", |state, value, _| {
        let tagged = !tags_of(state, &value).is_none();
        Ok(Value::from(true_type(&value, tagged)))
    });
    filter(env, "unmask", |_, value, args| {
        let names: Value = args
            .get("type_names")
            .map_err(|_| Error::new(ErrorKind::MissingArgument, "missing argument 'type_names'"))?;
        let _ = args.assert_all_used();
        let listed: Vec<String> = match names.kind() {
            ValueKind::String => vec![names.to_string()],
            ValueKind::Seq => names
                .try_iter()?
                .map(|n| n.as_str().map_or_else(|| n.to_string(), str::to_string))
                .collect(),
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidOperation,
                    format!(
                        "The 'type_names' argument must be of type 'str' or 'list', not '{}'.",
                        true_type(&names, false)
                    ),
                ));
            }
        };
        let unknown: Vec<&str> = listed
            .iter()
            .map(String::as_str)
            .filter(|n| !MASKED.contains(n))
            .collect();
        if !unknown.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                format!("Unknown type name(s): {}", unknown.join(", ")),
            ));
        }
        Ok(value)
    });
    env.add_test(
        format!("{NAMESPACE}.tagged"),
        |state: &State, value: Value| !tags_of(state, &value).is_none(),
    );
    env.add_test(
        format!("{NAMESPACE}.tagged_with"),
        |state: &State, value: Value, name: String| {
            if !TAG_NAMES.contains(&name.as_str()) {
                return Err(Error::new(
                    ErrorKind::InvalidOperation,
                    format!("Unknown tag name '{name}'."),
                ));
            }
            Ok(crate::tags::names(tags_of(state, &value)).contains(&name.as_str()))
        },
    );
}

/// The Python class of `value`, `_AnsibleTagged…` for one that carries tags.
fn true_type(value: &Value, tagged: bool) -> String {
    let base = match value.kind() {
        ValueKind::Undefined => return "UndefinedMarker".into(),
        ValueKind::None => return "NoneType".into(),
        ValueKind::Bool => return "bool".into(),
        ValueKind::String => "str",
        ValueKind::Number if value.is_integer() => "int",
        ValueKind::Number => "float",
        ValueKind::Seq if value.is_tuple() => "tuple",
        ValueKind::Seq => "list",
        ValueKind::Map => "dict",
        _ => return "object".into(),
    };
    if !tagged {
        return base.into();
    }
    let mut chars = base.chars();
    let first = chars
        .next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or_default();
    format!("_AnsibleTagged{first}{}", chars.as_str())
}

/// A set with only `origin`: what a value an engine source made carries.
pub(crate) fn origin_only(origin: crate::tags::OriginId) -> TagSetId {
    crate::tags::intern(TagSet {
        origin: Some(origin),
        ..TagSet::default()
    })
}
