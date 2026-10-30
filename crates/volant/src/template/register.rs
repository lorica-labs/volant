// SPDX-License-Identifier: GPL-3.0-or-later
//! Registering the engine's own filters and tests: each under its short name and its
//! `ansible.builtin` and `ansible.legacy` names, with the reference's handling of undefined
//! arguments, and its wording for an error the plugin raises.

use minijinja::functions::Function;
use minijinja::value::{FunctionArgs, FunctionResult, Rest, ValueOrKwargs};
use minijinja::{Environment, Error, ErrorKind, State};

use super::env::{any_undefined, holds_undefined};
use super::pymethods::mutable::{is_mutable, snapshot_answer};

/// An error a native filter or test raised, as ansible-core reports one its plugin raises:
/// `The filter plugin 'ansible.builtin.<name>' failed: <why>`, which the task's chain names as
/// the value's error. An undefined read stays one, and an error already worded is kept.
fn plugin_failure(kind: &str, name: &str, err: Error) -> Error {
    if err.kind() == ErrorKind::UndefinedError || super::bridged::said(&err).is_some() {
        return err;
    }
    let why = err
        .detail()
        .map_or_else(|| err.kind().to_string(), str::to_owned);
    if super::bridged::is_plugin_failure(&why) {
        return super::bridged::raise(why);
    }
    super::bridged::raise(format!(
        "The {kind} plugin 'ansible.builtin.{name}' failed: {why}"
    ))
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
            return Err(Error::from(ErrorKind::UndefinedError));
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
) -> impl Fn(&mut State, Rest<ValueOrKwargs>) -> Result<minijinja::Value, Error>
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
    let name = name.to_owned();
    let checked = match name.as_str() {
        "default" | "d" | "mandatory" | "type_debug" => 0,
        "ternary" => 1,
        _ => usize::MAX,
    };
    move |state: &mut State, args: Rest<ValueOrKwargs>| {
        let args = args.into_values();
        if any_undefined(&args[..checked.min(args.len())]) {
            return Ok(minijinja::Value::UNDEFINED);
        }
        let answer = f
            .call(state, &args)
            .map_err(|e| plugin_failure("filter", &name, e))?;
        // ansible-core hands back a copy of what a filter answers, so a list changed in place
        // does not change through what a filter made of it.
        Ok(if args.iter().any(is_mutable) {
            snapshot_answer(&answer)
        } else {
            answer
        })
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
    let owned = name.to_owned();
    let test = move |state: &mut State, args: Rest<ValueOrKwargs>| {
        let args = args.into_values();
        if !sees_undefined && any_undefined(&args) {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        f.call(state, &args)
            .map_err(|e| plugin_failure("test", &owned, e))
    };
    env.add_test(format!("ansible.builtin.{name}"), test.clone());
    env.add_test(format!("ansible.legacy.{name}"), test.clone());
    env.add_test(name.to_string(), test);
}
