// SPDX-License-Identifier: GPL-3.0-or-later
//! The Jinja globals that build a value: `dict`, `cycler`, `joiner` and `undef`.
//!
//! minijinja's `dict` takes a mapping and keywords only. Python's `dict`, which Jinja hands to
//! templates unchanged, also takes any iterable of pairs, and Ansible roles rely on it
//! (`dict(items)`, `dict(keys | zip(values))`). Its failures are worded as CPython words them.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use minijinja::value::{Object, ObjectRepr, Rest, ValueKind, ValueOrKwargs, from_args};
use minijinja::{Error, ErrorKind, State, Value};

use super::{any_undefined, type_name};

/// What makes two keys the same key in a Python `dict`: `1`, `1.0` and `true` are one key. An
/// integer is held whole, so two integers beyond 64 bits stay two keys.
#[derive(Hash, PartialEq, Eq)]
enum Key {
    Int(i128),
    Text(String),
}

/// The most pairs `dict()` takes from one iterable. A generator of unknown length would otherwise
/// grow the result until the process runs out of memory; ten million is far above what a
/// playbook builds and a few hundred megabytes at most.
const MAX_PAIRS: usize = 10_000_000;

/// A `dict` under construction: the keys in the order they were first set, the value last set.
#[derive(Default)]
struct Entries {
    items: Vec<(Value, Value)>,
    index: HashMap<Key, usize>,
}

impl Entries {
    /// A key that is text, a boolean or a whole number. `none` and a number with a fraction (or
    /// beyond any integer) are keys Python accepts and this engine cannot carry through a
    /// template result: they would reach the output as `null` or fail there, depending on the
    /// consumer. They are refused here, where the cause is still known.
    fn set(&mut self, key: Value, value: Value) -> Result<(), Error> {
        let canonical = match key.kind() {
            ValueKind::Bool | ValueKind::Number => match i128::try_from(key.clone()) {
                Ok(whole) => Key::Int(whole),
                Err(_) => {
                    return Err(invalid(
                        "a dict key of type 'float' is not supported by this release, only text, \
                         boolean and whole number keys"
                            .into(),
                    ));
                }
            },
            ValueKind::None => {
                return Err(invalid(
                    "a dict key of type 'NoneType' is not supported by this release, only text, \
                     boolean and whole number keys"
                        .into(),
                ));
            }
            ValueKind::String => Key::Text(key.to_string()),
            ValueKind::Undefined => return Err(Error::from(ErrorKind::UndefinedError)),
            _ => {
                return Err(invalid(format!("unhashable type: '{}'", python_type(&key))));
            }
        };
        if let Some(&at) = self.index.get(&canonical) {
            self.items[at].1 = value;
        } else {
            self.index.insert(canonical, self.items.len());
            self.items.push((key, value));
        }
        Ok(())
    }
}

fn invalid(message: String) -> Error {
    Error::new(ErrorKind::InvalidOperation, message)
}

/// The name Python gives the type of a value, as the messages of `dict` quote it.
pub(super) fn python_type(value: &Value) -> &'static str {
    match value.kind() {
        ValueKind::None => "NoneType",
        ValueKind::Bool => "bool",
        ValueKind::Number => serde_json::to_value(value).map_or("int", |json| type_name(&json)),
        _ if super::bigint::is_big(value) => "int",
        ValueKind::String => "str",
        ValueKind::Bytes => "bytes",
        ValueKind::Seq => "list",
        ValueKind::Map => "dict",
        ValueKind::Iterable => "generator",
        _ => "object",
    }
}

/// The items of a sequence, a generator or a string, one at a time; for a mapping, its keys.
/// Nothing is collected, so an iterable that is not made of pairs fails at its first element.
fn items_of(value: &Value) -> Result<Box<dyn Iterator<Item = Value>>, Error> {
    if value.kind() == ValueKind::String {
        let chars: Vec<Value> = value
            .to_string()
            .chars()
            .map(|c| Value::from(c.to_string()))
            .collect();
        return Ok(Box::new(chars.into_iter()));
    }
    Ok(Box::new(value.try_iter()?))
}

/// The `dict` global: it builds a mapping when called, and has the class method `fromkeys`, which
/// Python writes `dict.fromkeys(keys, value)`.
#[derive(Debug)]
pub(super) struct DictType;

impl Object for DictType {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<class 'dict'>")
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        (key.as_str()? == "fromkeys").then(|| Value::from_object(FromKeys))
    }

    /// `dict(...)`: nothing, a mapping, or any iterable of two-item elements, then keywords.
    ///
    /// Measured on ansible-core 2.19.12: a later pair wins over an earlier one with the same key,
    /// a keyword wins over the positional argument, `dict(none)` and `dict(5)` fail as non-iterable
    /// and a string is read character by character.
    fn call(self: &Arc<Self>, _state: &mut State<'_, '_>, args: &[Value]) -> Result<Value, Error> {
        build(args.to_vec(), MAX_PAIRS)
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        if method != "fromkeys" {
            return Err(Error::from(ErrorKind::UnknownMethod));
        }
        from_keys(Rest(args.to_vec()))
    }
}

/// The bound method `dict.fromkeys`, which prints as Python prints a built-in method.
#[derive(Debug)]
struct FromKeys;

impl Object for FromKeys {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<built-in method fromkeys of type object>")
    }

    fn call(self: &Arc<Self>, _state: &mut State<'_, '_>, args: &[Value]) -> Result<Value, Error> {
        from_keys(Rest(args.to_vec()))
    }
}

/// The Python type name of a template value that is a type or a function, which no result can
/// hold: ansible-core refuses to store one.
pub(super) fn unstorable(value: &Value) -> Option<&'static str> {
    if value.downcast_object_ref::<DictType>().is_some() {
        Some("type")
    } else if value.downcast_object_ref::<FromKeys>().is_some() {
        Some("builtin_function_or_method")
    } else {
        None
    }
}

/// `dict.fromkeys(iterable, value=none)`. An undefined argument is an undefined read.
fn from_keys(args: Rest<Value>) -> Result<Value, Error> {
    if any_undefined(&args) {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    super::pymethods::fromkeys(&args)
}

/// The entries of `dict.fromkeys`: each of `keys` with `value`. The keys are told apart as
/// `dict()` tells them apart: `1`, `1.0` and `true` are one key.
pub(super) fn keyed(keys: Vec<Value>, value: &Value) -> Result<Vec<(Value, Value)>, Error> {
    let mut entries = Entries::default();
    for key in keys {
        entries.set(key, value.clone())?;
    }
    Ok(entries.items)
}

/// `dict(...)` with the most pairs it takes from one iterable as a parameter.
pub(super) fn build(mut args: Vec<Value>, max_pairs: usize) -> Result<Value, Error> {
    let keywords = match args.last() {
        Some(last) if last.is_kwargs() => args.pop(),
        _ => None,
    };
    if args.len() > 1 {
        return Err(invalid(format!(
            "dict expected at most 1 argument, got {}",
            args.len()
        )));
    }
    // A mapping and no keywords: the copy is the mapping. Values are never changed in place in a
    // template, and the mapping may be lazy (`vars`, `hostvars`), where a copy would read, and
    // fail on, every variable the caller never asked for.
    if keywords.is_none()
        && let Some(source) = args.first()
        && source.kind() == ValueKind::Map
    {
        return Ok(source.clone());
    }
    let mut entries = Entries::default();
    if let Some(source) = args.first() {
        match source.kind() {
            ValueKind::Undefined => return Err(Error::from(ErrorKind::UndefinedError)),
            ValueKind::Map => {
                for key in source.try_iter()? {
                    // A value that cannot be read is undefined in the copy, as it is in the
                    // mapping itself.
                    let value = source.get_item(&key).unwrap_or(Value::UNDEFINED);
                    entries.set(key, value)?;
                }
            }
            ValueKind::Seq | ValueKind::Iterable | ValueKind::String => {
                for (at, element) in items_of(source)?.enumerate() {
                    if at >= max_pairs {
                        return Err(invalid(format!(
                            "dict() takes at most {max_pairs} pairs from one iterable"
                        )));
                    }
                    let (key, value) = pair(at, &element)?;
                    entries.set(key, value)?;
                }
            }
            _ => {
                return Err(invalid(format!(
                    "'{}' object is not iterable",
                    python_type(source)
                )));
            }
        }
    }
    if let Some(keywords) = keywords {
        for key in keywords.try_iter()? {
            let value = keywords.get_item(&key)?;
            entries.set(key, value)?;
        }
    }
    Ok(Value::from_pairs(entries.items))
}

/// The two items of element number `at`.
fn pair(at: usize, element: &Value) -> Result<(Value, Value), Error> {
    if element.is_undefined() {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    let wrong_length = |length: usize| {
        invalid(format!(
            "dictionary update sequence element #{at} has length {length}; 2 is required"
        ))
    };
    match element.kind() {
        ValueKind::Seq | ValueKind::Iterable | ValueKind::String | ValueKind::Map => {}
        _ => {
            return Err(invalid(format!(
                "cannot convert dictionary update sequence element #{at} to a sequence"
            )));
        }
    }
    // The length is read without walking the element when the element knows it.
    if let Some(length) = element.len()
        && length != 2
    {
        return Err(wrong_length(length));
    }
    let mut items = items_of(element)?;
    let Some(key) = items.next() else {
        return Err(wrong_length(0));
    };
    let Some(value) = items.next() else {
        return Err(wrong_length(1));
    };
    let more = items.count();
    if more > 0 {
        return Err(wrong_length(2 + more));
    }
    Ok((key, value))
}

/// Jinja's `cycler`: `next()` answers the current item and moves on, `current` reads it,
/// `reset()` starts over, and the items repeat for ever.
#[derive(Debug)]
struct Cycler {
    items: Vec<Value>,
    pos: AtomicUsize,
}

impl Object for Cycler {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        match key.as_str()? {
            "current" => Some(self.items[self.pos.load(Ordering::Relaxed)].clone()),
            "pos" => Some(Value::from(self.pos.load(Ordering::Relaxed))),
            "items" => Some(Value::from(self.items.clone())),
            _ => None,
        }
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let () = from_args(args)?;
        let at = self.pos.load(Ordering::Relaxed);
        match method {
            "next" => {
                self.pos
                    .store((at + 1) % self.items.len(), Ordering::Relaxed);
                Ok(self.items[at].clone())
            }
            "reset" => {
                self.pos.store(0, Ordering::Relaxed);
                Ok(Value::from(()))
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

/// `cycler(*items)`. At least one item is needed, as in Jinja.
pub(super) fn cycler(items: Rest<Value>) -> Result<Value, Error> {
    if items.is_empty() {
        return Err(invalid("at least one item has to be provided".into()));
    }
    Ok(Value::from_object(Cycler {
        items: items.0,
        pos: AtomicUsize::new(0),
    }))
}

/// `undef(hint=none, name=none)`: a value that is undefined, so that reading it fails and
/// `default` and `is defined` see it as missing. The hint is not kept: the failure it causes
/// reads as any other undefined variable.
pub(super) fn undef(_args: Rest<ValueOrKwargs>) -> Value {
    Value::UNDEFINED
}

/// What a call is bound to. Python words the failure of a function written in Python and of one
/// written in C (`strftime`, `isoformat`) in two ways.
#[derive(Clone, Copy)]
pub(super) enum Callee {
    Python,
    Native,
}

/// The arguments of a call, by position then by keyword, in the order of `names`, as Python binds
/// them. An undefined argument is an undefined read, as for any plugin the reference calls.
pub(super) fn bind<const N: usize>(
    callee: Callee,
    name: &str,
    names: &[&str; N],
    args: &[Value],
) -> Result<[Option<Value>; N], Error> {
    if any_undefined(args) {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    let (positional, keywords) = match args.split_last() {
        Some((last, rest)) if last.is_kwargs() => (rest, Some(last)),
        _ => (args, None),
    };
    if positional.len() > N {
        let given = positional.len();
        return Err(invalid(match callee {
            Callee::Python => {
                format!("{name}() takes from 0 to {N} positional arguments but {given} were given")
            }
            Callee::Native => format!(
                "{name}() takes at most {N} argument{} ({given} given)",
                if N == 1 { "" } else { "s" }
            ),
        }));
    }
    let mut bound: [Option<Value>; N] = std::array::from_fn(|at| positional.get(at).cloned());
    if let Some(keywords) = keywords {
        for key in keywords.try_iter()? {
            let word = key.to_string();
            let Some(at) = names.iter().position(|known| *known == word) else {
                return Err(invalid(match callee {
                    Callee::Python => {
                        format!("{name}() got an unexpected keyword argument '{word}'")
                    }
                    Callee::Native => {
                        format!("'{word}' is an invalid keyword argument for {name}()")
                    }
                }));
            };
            if bound[at].is_some() {
                return Err(invalid(match callee {
                    Callee::Python => format!("{name}() got multiple values for argument '{word}'"),
                    Callee::Native => format!(
                        "argument for {name}() given by name ('{word}') and position ({})",
                        at + 1
                    ),
                }));
            }
            bound[at] = Some(keywords.get_item(&key)?);
        }
    }
    Ok(bound)
}
