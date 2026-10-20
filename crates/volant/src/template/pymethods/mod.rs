// SPDX-License-Identifier: GPL-3.0-or-later
//! Python's methods on the values a template calls them on: `'a,b'.split(',')`,
//! `'{:>5}'.format(x)`. minijinja has no such methods of its own, so the environment's
//! unknown-method hook answers them. A name no table below holds fails by its name.

use std::sync::Arc;

use minijinja::value::{Tuple, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State, Value};

mod args;
pub(crate) mod bytes;
mod codec;
mod dicts;
mod digits;
pub(crate) mod format;
pub(crate) mod limits;
mod lists;
pub(super) mod mutable;
mod percent;
mod search;
mod text;
mod translate;

pub(super) use args::type_of;
use args::{Call, raise};
pub(in crate::template) use dicts::fromkeys;
use mutable::{Dict, MutDict, MutList, is_dict, is_list};
pub(super) use percent::{format_filter, percent};

/// Installs the hook on `env`.
pub(super) fn register(env: &mut Environment<'static>) {
    // `pycompat` still answers `dict` and `list` methods. A method it does not know comes back
    // as a bare `UnknownMethod`, and one with a bad argument keeps its own detail.
    env.set_unknown_method_callback(|state, value, method, args| {
        call(state, value, method, args).map_err(|e| {
            if e.kind() == ErrorKind::UnknownMethod && e.detail().is_none() {
                Error::new(
                    ErrorKind::UnknownMethod,
                    format!("method {method} is not available yet"),
                )
            } else {
                e
            }
        })
    });
}

fn call(state: &mut State, value: &Value, method: &str, args: &[Value]) -> Result<Value, Error> {
    if value.kind() == ValueKind::String
        && let Some(s) = value.as_str()
    {
        if super::any_undefined(args) {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        if let Some(answer) = string(s, method, args) {
            return answer;
        }
        if !STR_METHODS.contains(&method) {
            return Err(raise(format!(
                "object of type 'str' has no attribute '{method}'"
            )));
        }
    }
    if let Some(held) = bytes::of(value) {
        if super::any_undefined(args) {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        return bytes::call(held, method, args)
            .unwrap_or_else(|| Err(Error::from(ErrorKind::UnknownMethod)));
    }
    if let Some(tuple) = value.downcast_object_ref::<Tuple>() {
        if super::any_undefined(args) {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        if matches!(method, "index" | "count")
            && let Some(answer) =
                lists::call(&MutList::temp(Arc::new(tuple.to_vec())), method, args)
        {
            return answer;
        }
        return Err(raise(format!(
            "object of type 'tuple' has no attribute '{method}'"
        )));
    }
    if matches!(value.kind(), ValueKind::Seq | ValueKind::Iterable)
        && !is_list(value)
        && mutable::MUTATORS.contains(&method)
        && method != "update"
    {
        return Err(raise(
            "a list built with + or a slice cannot be changed in place: use `| list` first",
        ));
    }
    if is_list(value) || is_dict(value) {
        if super::any_undefined(args) {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        if let Some(answer) = container(value, method, args) {
            return answer;
        }
        return Err(raise(format!(
            "object of type '{}' has no attribute '{method}'",
            type_of(value)
        )));
    }
    minijinja_contrib::pycompat::unknown_method_callback(state, value, method, args)
}

/// A method of `list` or `dict` called on a value that is not one of the mutable objects, which
/// a template reaches by a path that does not go through a variable (`(x | list).append(1)`,
/// `hostvars[h].items.append(1)`). The method runs on a copy, so what it returns is right and
/// what it changes is lost.
fn container(value: &Value, method: &str, args: &[Value]) -> Option<Result<Value, Error>> {
    let object = value.as_object()?;
    if let Some(items) = object.downcast::<Vec<Value>>() {
        return lists::call(&MutList::temp(items), method, args);
    }
    // A mutable list or mapping reaches here for a name that no method has.
    object
        .downcast::<Dict>()
        .and_then(|entries| dicts::call(&MutDict::temp(entries), method, args))
}

/// Every method `str` has in CPython 3.13. A name outside this list is no method at all; one in
/// it that this engine does not run fails as not available yet.
const STR_METHODS: [&str; 47] = [
    "capitalize",
    "casefold",
    "center",
    "count",
    "encode",
    "endswith",
    "expandtabs",
    "find",
    "format",
    "format_map",
    "index",
    "isalnum",
    "isalpha",
    "isascii",
    "isdecimal",
    "isdigit",
    "isidentifier",
    "islower",
    "isnumeric",
    "isprintable",
    "isspace",
    "istitle",
    "isupper",
    "join",
    "ljust",
    "lower",
    "lstrip",
    "maketrans",
    "partition",
    "removeprefix",
    "removesuffix",
    "replace",
    "rfind",
    "rindex",
    "rjust",
    "rpartition",
    "rsplit",
    "rstrip",
    "split",
    "splitlines",
    "startswith",
    "strip",
    "swapcase",
    "title",
    "translate",
    "upper",
    "zfill",
];

/// The methods of `str` this engine runs; `None` for a name it does not.
fn string(s: &str, name: &str, args: &[Value]) -> Option<Result<Value, Error>> {
    let call = Call::new(args);
    Some(match name {
        "split" => text::split(s, name, &call, false),
        "rsplit" => text::split(s, name, &call, true),
        "strip" | "lstrip" | "rstrip" => text::strip(s, name, &call),
        "center" | "ljust" | "rjust" => text::pad(s, name, &call),
        "zfill" => text::zfill(s, &call),
        "splitlines" => text::splitlines(s, &call),
        "expandtabs" => text::expandtabs(s, &call),
        "upper" | "lower" | "casefold" | "swapcase" | "capitalize" | "title" => call
            .none(name)
            .map(|()| Value::from(text::case(s, name).unwrap_or_default())),
        "isdigit" | "isdecimal" | "isnumeric" | "isalpha" | "isalnum" | "isspace" | "isascii"
        | "islower" | "isupper" | "istitle" | "isidentifier" | "isprintable" => call
            .none(name)
            .map(|()| Value::from(text::predicate(s, name).unwrap_or_default())),
        "find" | "rfind" | "index" | "rindex" => search::find(s, name, &call),
        "count" => search::count(s, &call),
        "startswith" | "endswith" => search::affix_match(s, name, &call),
        "replace" => search::replace(s, &call),
        "join" => search::join(s, &call),
        "partition" | "rpartition" => search::partition(s, name, &call),
        "removeprefix" => call
            .one(name)
            .and_then(|v| text::affix(name, v))
            .map(|p| Value::from(s.strip_prefix(p).unwrap_or(s))),
        "removesuffix" => call
            .one(name)
            .and_then(|v| text::affix(name, v))
            .map(|p| Value::from(s.strip_suffix(p).unwrap_or(s))),
        "format" => format::format(
            s,
            &format::Source {
                pos: call.pos,
                kw: &call.kw,
                map: None,
            },
        )
        .map(Value::from),
        "format_map" => call.one(name).and_then(|map| {
            format::format(
                s,
                &format::Source {
                    pos: &[],
                    kw: &[],
                    map: Some(map),
                },
            )
            .map(Value::from)
        }),
        "translate" => call
            .one(name)
            .and_then(|table| translate::translate(s, table)),
        "maketrans" => translate::maketrans(&call),
        "encode" => bytes::encode(s, &call),
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
