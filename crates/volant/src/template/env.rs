// SPDX-License-Identifier: GPL-3.0-or-later
//! The minijinja environment of the engine, and the helpers that register Ansible's filters and
//! tests on it.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use minijinja::functions::Function;
use minijinja::value::{FunctionArgs, FunctionResult, Rest, ValueKind, ValueOrKwargs};
use minijinja::{Environment, ErrorKind, State, UndefinedBehavior};
use serde_json::Value;

use crate::bridge::Bridge;

use super::clock::DateTime;
use super::context::Context;
use super::filters;
use super::globals;
use super::pymethods::mutable::{
    MutDict, MutList, is_literal, is_mutable, key, mutable, snapshot_answer,
};
use super::pymethods::percent;
use super::vm::{concat, note_emit};
use super::{
    ATTR_KEY, CONCAT_KEY, CONST_KEY, CONTEXT_KEY, ITEM_KEY, MATTR_KEY, MITEM_KEY, MKLIST_KEY,
    MKMAP_KEY, REM_KEY, REMT_KEY, VAR_KEY,
};

/// The engine's environment: minijinja's, strict about undefined values, with every filter,
/// test and lookup of the engine; the lookups the engine lacks go to `bridge`.
pub(super) fn environment(base_dir: PathBuf, bridge: Option<&Arc<Bridge>>) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    // Strict mode refuses to print an undefined value, not a list or mapping holding one,
    // which minijinja prints as `[undefined]`. Measured on ansible-core 2.19.12,
    // `x {{ [nope] }}` is an undefined read.
    //
    // Inside text, measured on the same version: a list or mapping prints as Python's `repr`
    // does, and a `none` that is a whole part of the text prints nothing.
    env.set_formatter(|out, state, value| {
        if !value.is_undefined() && holds_undefined(value) {
            return Err(minijinja::Error::from(ErrorKind::UndefinedError));
        }
        note_emit(out, value);
        if value.is_none() {
            return Ok(());
        }
        // Bytes print as the text they hold, as ansible-core joins them into the text.
        if let Some(bytes) = super::pymethods::bytes::of(value) {
            let text = bytes
                .text()
                .map_err(|e| minijinja::Error::new(ErrorKind::InvalidOperation, e))?;
            return out.write_str(text).map_err(|_| {
                minijinja::Error::new(ErrorKind::WriteFailure, "failed to write output")
            });
        }
        match container_repr(value) {
            Some(text) => out.write_str(&text).map_err(|_| {
                minijinja::Error::new(ErrorKind::WriteFailure, "failed to write output")
            }),
            None => minijinja::escape_formatter(out, state, value),
        }
    });
    filters::register(&mut env, base_dir, bridge);
    super::order::register(&mut env);
    env.add_function(CONCAT_KEY, concat);
    env.add_function("range", range);
    env.add_global("dict", minijinja::Value::from_object(globals::DictType));
    env.add_function("cycler", globals::cycler);
    env.add_function("joiner", minijinja_contrib::globals::joiner);
    env.add_function("undef", globals::undef);
    env.add_function("now", super::clock::now);
    env.add_function("lipsum", super::lipsum::lipsum);
    env.add_function(ITEM_KEY, chained_item);
    env.add_function(MITEM_KEY, mutable_item);
    env.add_function(MKLIST_KEY, |items: Rest<minijinja::Value>| {
        MutList::from_items(items.0)
    });
    env.add_function(
        MKMAP_KEY,
        |pairs: Rest<minijinja::Value>| -> Result<minijinja::Value, minijinja::Error> {
            let mut entries = indexmap::IndexMap::new();
            for pair in pairs.chunks(2) {
                entries.insert(key(&pair[0])?, pair[1].clone());
            }
            Ok(MutDict::from_entries(entries))
        },
    );
    env.add_function(REM_KEY, |l: minijinja::Value, r: minijinja::Value| {
        percent(&l, &r, false)
    });
    env.add_function(REMT_KEY, |l: minijinja::Value, r: minijinja::Value| {
        percent(&l, &r, true)
    });
    env
}

/// Python's `repr` of a list or mapping, and `str` of a `datetime`, which has two forms; `None`
/// for any other value.
pub(super) fn container_repr(value: &minijinja::Value) -> Option<String> {
    if let Some(moment) = value.downcast_object_ref::<DateTime>() {
        return Some(moment.python_str());
    }
    if let Some(range) = value.downcast_object_ref::<Range>() {
        return Some(range.to_string());
    }
    if super::pymethods::bytes::holds(value) {
        return Some(super::pymethods::format::repr(value));
    }
    if !matches!(
        value.kind(),
        ValueKind::Seq | ValueKind::Map | ValueKind::Iterable
    ) {
        return None;
    }
    let json = super::bigint::to_json(value).ok()?;
    Some(crate::playbook::python_repr(&json))
}

/// Attribute access that leaves an undefined parent undefined, as the reference does until
/// `default`, `is defined` or a use decides. The VM's own lookup fails on an undefined parent.
pub(super) fn chained_attr(name: String) -> minijinja::Value {
    minijinja::Value::from_function(
        move |parent: minijinja::Value| -> Result<minijinja::Value, minijinja::Error> {
            if parent.is_undefined() {
                return Ok(minijinja::Value::UNDEFINED);
            }
            parent.get_attr(&name)
        },
    )
}

/// `parent[key]`, chained like [`chained_attr`].
fn chained_item(
    parent: minijinja::Value,
    key: minijinja::Value,
) -> Result<minijinja::Value, minijinja::Error> {
    if parent.is_undefined() {
        return Ok(minijinja::Value::UNDEFINED);
    }
    parent.get_item(&key)
}

/// The function name an attribute lookup is routed through: `ATTR_KEY` then the attribute.
pub(super) fn attr_call(name: &str, mutable: bool) -> &'static str {
    routed(if mutable { MATTR_KEY } else { ATTR_KEY }, name)
}

/// The function name a variable read is routed through in a template that changes values in
/// place: `VAR_KEY` then the name.
pub(super) fn var_call(name: &str) -> &'static str {
    routed(VAR_KEY, name)
}

/// The function name a list or mapping constant is routed through, or `None` when the constant is
/// not one that JSON carries back unchanged (a mapping with a key that is not a string).
pub(super) fn const_call(value: &minijinja::Value) -> Option<&'static str> {
    if !is_literal(value) {
        return None;
    }
    Some(routed(CONST_KEY, &serde_json::to_string(value).ok()?))
}

/// What a constant routed by [`const_call`] answers: a new mutable list or mapping.
pub(super) fn fresh_const(json: &str) -> Option<minijinja::Value> {
    let parsed: Value = serde_json::from_str(json).ok()?;
    Some(minijinja::Value::from_function(
        move || -> minijinja::Value {
            let plain = super::bigint::to_template(&parsed);
            mutable(&plain).unwrap_or(plain)
        },
    ))
}

/// `prefix` and `name` as one name. Leaked once per distinct name, because compiled code holds
/// names for as long as it lives.
fn routed(prefix: &str, name: &str) -> &'static str {
    static NAMES: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    let full = format!("{prefix}{name}");
    let mut names = NAMES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(known) = names.get(full.as_str()) {
        return known;
    }
    let leaked: &'static str = Box::leak(full.into_boxed_str());
    names.insert(leaked);
    leaked
}

/// The render's root context, which keeps the mutable form of each plain value it handed out.
fn root(state: &State) -> Option<minijinja::Value> {
    state.lookup(CONTEXT_KEY)
}

/// The mutable form of a value a template read from a name, an attribute or an item.
pub(super) fn made_mutable(state: &State, value: minijinja::Value) -> minijinja::Value {
    root(state)
        .and_then(|ctx| {
            ctx.downcast_object_ref::<Context>()
                .map(|ctx| ctx.mutable_read(value.clone()))
        })
        .unwrap_or(value)
}

/// `chained_attr`, for a template that changes values in place.
pub(super) fn mutable_attr(name: String) -> minijinja::Value {
    minijinja::Value::from_function(
        move |state: &State,
              parent: minijinja::Value|
              -> Result<minijinja::Value, minijinja::Error> {
            if parent.is_undefined() {
                return Ok(minijinja::Value::UNDEFINED);
            }
            Ok(made_mutable(state, parent.get_attr(&name)?))
        },
    )
}

/// A variable read, for a template that changes values in place.
pub(super) fn mutable_var(name: String) -> minijinja::Value {
    minijinja::Value::from_function(move |state: &State| -> minijinja::Value {
        made_mutable(
            state,
            state.lookup(&name).unwrap_or(minijinja::Value::UNDEFINED),
        )
    })
}

/// `chained_item`, for a template that changes values in place.
fn mutable_item(
    state: &State,
    parent: minijinja::Value,
    key: minijinja::Value,
) -> Result<minijinja::Value, minijinja::Error> {
    if parent.is_undefined() {
        return Ok(minijinja::Value::UNDEFINED);
    }
    Ok(made_mutable(state, parent.get_item(&key)?))
}

pub(super) fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Registers a filter under `name` and again under `ansible.builtin.<name>`: every filter and
/// test ansible-core ships lives in that collection, and a role is free to spell it out in full.
/// Measured (A9): minijinja's parser accepts a dotted name after `|` or `is` as a single filter
/// or test name — `{{ "x" | ansible.builtin.default("y") }}` fails by `unknown filter: filter
/// ansible.builtin.default is unknown`, not by a syntax error — so registering the qualified
/// string is all a name needs. `ansible.utils.ipwrap` is registered on its own, never through
/// this: the reference never exposes it under `ansible.builtin`.
///
/// Measured on ansible-core 2.19.12, a filter given an undefined value does not run: the result
/// is undefined too, so `nope | dict2items` fails as an undefined read and
/// `nope | dict2items | default('x')` is `x`. The filter is wrapped here to do the same, except
/// the few that exist to look at an undefined value.
pub(crate) fn add_filter<F, Rv, Args>(env: &mut Environment<'static>, name: &str, f: F)
where
    F: Function<Rv, Args>,
    Rv: FunctionResult,
    Args: for<'a> FunctionArgs<'a>,
{
    let filter = undefined_through(name, f);
    env.add_filter(format!("ansible.builtin.{name}"), filter.clone());
    env.add_filter(format!("ansible.legacy.{name}"), filter.clone());
    env.add_filter(name.to_string(), filter);
}

/// [`add_filter`] for a filter that turns a container into text or a number (`join`, `string`,
/// `quote`, `sum`): an undefined value anywhere inside an argument fails as an undefined read.
/// Measured on ansible-core 2.19.12, `[1, nope] | join(',')` fails where `[nope] | length` and
/// `[1, nope] | first` answer `1`, so the filters that only count or pick keep [`add_filter`].
pub(crate) fn add_text_filter<F, Rv, Args>(env: &mut Environment<'static>, name: &str, f: F)
where
    F: Function<Rv, Args>,
    Rv: FunctionResult,
    Args: for<'a> FunctionArgs<'a>,
{
    let f = undefined_through(name, f);
    let filter = move |state: &mut State, args: Rest<ValueOrKwargs>| {
        if args.iter().any(|v| !v.is_undefined() && holds_undefined(v)) {
            return Err(minijinja::Error::from(ErrorKind::UndefinedError));
        }
        f(state, args)
    };
    env.add_filter(format!("ansible.builtin.{name}"), filter.clone());
    env.add_filter(format!("ansible.legacy.{name}"), filter.clone());
    env.add_filter(name.to_string(), filter);
}

/// `f`, answering undefined when an argument it is given is undefined; see [`add_filter`].
/// Measured on ansible-core 2.19.12: any argument counts, keyword ones included
/// (`'%s' | format(nope | lower)`, `regex_replace('a', 'b', ignorecase=nope | bool)` are
/// undefined reads), but only the argument itself, not what it holds (`[nope | lower] | length`
/// is 1). `ternary` only looks at its input (`true | ternary('a', nope)` is `a`); `default`,
/// `d`, `mandatory` and `type_debug` look at nothing.
pub(crate) fn undefined_through<F, Rv, Args>(
    name: &str,
    f: F,
) -> impl Fn(&mut State, Rest<ValueOrKwargs>) -> Result<minijinja::Value, minijinja::Error>
+ Clone
+ Send
+ Sync
+ 'static
where
    F: Function<Rv, Args>,
    Rv: FunctionResult,
    Args: for<'a> FunctionArgs<'a>,
{
    let f = minijinja::Value::from_function(f);
    let checked = match name {
        "default" | "d" | "mandatory" | "type_debug" => 0,
        "ternary" => 1,
        _ => usize::MAX,
    };
    move |state: &mut State, args: Rest<ValueOrKwargs>| {
        let args = args.into_values();
        if any_undefined(&args[..checked.min(args.len())]) {
            return Ok(minijinja::Value::UNDEFINED);
        }
        let answer = f.call(state, &args)?;
        // ansible-core hands back a copy of what a filter answers, so a list changed in place
        // does not change through what a filter made of it.
        Ok(if args.iter().any(is_mutable) {
            snapshot_answer(&answer)
        } else {
            answer
        })
    }
}

/// Whether one of these arguments, or one keyword argument among them, is undefined.
pub(super) fn any_undefined(args: &[minijinja::Value]) -> bool {
    args.iter().any(|v| {
        v.is_undefined()
            || (v.is_kwargs()
                && v.try_iter().is_ok_and(|mut keys| {
                    keys.any(|k| v.get_item(&k).is_ok_and(|x| x.is_undefined()))
                }))
    })
}

/// Whether a value is undefined or holds an undefined value in one of its lists or mappings.
/// minijinja keeps an undefined value inside a literal (`[nope]`, `{'k': nope | int}`) or a
/// concatenation (`items + [nope]`, a lazy sequence) and serialises it as `null`; the reference fails on it as an undefined read (measured on
/// ansible-core 2.19.12), so every place a value leaves the template engine asks this first.
pub(crate) fn holds_undefined(v: &minijinja::Value) -> bool {
    match v.kind() {
        ValueKind::Undefined => true,
        ValueKind::Seq | ValueKind::Iterable => v
            .try_iter()
            .is_ok_and(|mut it| it.any(|x| holds_undefined(&x))),
        ValueKind::Map => v
            .try_iter()
            .is_ok_and(|mut keys| keys.any(|k| v.get_item(&k).is_ok_and(|x| holds_undefined(&x)))),
        _ => false,
    }
}

/// The test equivalent of [`add_filter`]. A test given an undefined argument fails as an
/// undefined read (`nope is none`, measured), except `defined` and `undefined`.
pub(crate) fn add_test<F, Rv, Args>(env: &mut Environment<'static>, name: &str, f: F)
where
    F: Function<Rv, Args>,
    Rv: FunctionResult,
    Args: for<'a> FunctionArgs<'a>,
{
    let f = minijinja::Value::from_function(f);
    let sees_undefined = ["defined", "undefined"].contains(&name);
    let test = move |state: &mut State, args: Rest<ValueOrKwargs>| {
        let args = args.into_values();
        if !sees_undefined && any_undefined(&args) {
            return Err(minijinja::Error::from(ErrorKind::UndefinedError));
        }
        f.call(state, &args)
    };
    env.add_test(format!("ansible.builtin.{name}"), test.clone());
    env.add_test(format!("ansible.legacy.{name}"), test.clone());
    env.add_test(name.to_string(), test);
}

/// Python truthiness, for messages and for filters that need it.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `range`: it iterates like a list and prints as `range(0, 3)`.
#[derive(Debug)]
pub(super) struct Range {
    start: i64,
    stop: i64,
    step: i64,
}

impl Range {
    fn len(&self) -> usize {
        let span = if self.step > 0 {
            self.stop - self.start
        } else {
            self.start - self.stop
        };
        if span <= 0 {
            0
        } else {
            usize::try_from((span - 1) / self.step.abs() + 1).unwrap_or(0)
        }
    }
}

impl std::fmt::Display for Range {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.step == 1 {
            write!(f, "range({}, {})", self.start, self.stop)
        } else {
            write!(f, "range({}, {}, {})", self.start, self.stop, self.step)
        }
    }
}

impl minijinja::value::Object for Range {
    fn repr(self: &Arc<Self>) -> minijinja::value::ObjectRepr {
        minijinja::value::ObjectRepr::Seq
    }

    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let at = i64::try_from(key.as_usize()?).ok()?;
        (usize::try_from(at).ok()? < self.len())
            .then(|| minijinja::Value::from(self.start + at * self.step))
    }

    fn enumerate(self: &Arc<Self>) -> minijinja::value::Enumerator {
        minijinja::value::Enumerator::Seq(self.len())
    }
}

fn range(args: Rest<i64>) -> Result<minijinja::Value, minijinja::Error> {
    let (start, stop, step) = match args.0[..] {
        [stop] => (0, stop, 1),
        [start, stop] => (start, stop, 1),
        [start, stop, step] => (start, stop, step),
        _ => {
            return Err(minijinja::Error::new(
                ErrorKind::InvalidOperation,
                "range expected 1 to 3 integer arguments",
            ));
        }
    };
    if step == 0 {
        return Err(minijinja::Error::new(
            ErrorKind::InvalidOperation,
            "range() arg 3 must not be zero",
        ));
    }
    Ok(minijinja::Value::from_object(Range { start, stop, step }))
}
