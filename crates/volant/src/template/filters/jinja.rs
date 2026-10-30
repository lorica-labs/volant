// SPDX-License-Identifier: GPL-3.0-or-later
//! Jinja2's `round`, `sum` and `replace`, with the arguments minijinja's own versions lack:
//! `round`'s `method`, `sum`'s `attribute` and `start`, `replace`'s `count`. A call without them
//! goes to minijinja's filter, which already rounds and adds as Python does.

use minijinja::filters as f;
use minijinja::value::{Kwargs, StringInput, ValueKind};
use minijinja::{Error, ErrorKind, State, Value};

fn failed(filter: &str, msg: &str) -> Error {
    crate::template::bridged::raise(format!(
        "The filter plugin 'ansible.builtin.{filter}' failed: {msg}"
    ))
}

/// `round(value, precision=0, method='common')`. `ceil` and `floor` give a float, as
/// `math.ceil(value * 10 ** precision) / 10 ** precision` does.
pub(super) fn round(
    value: Value,
    precision: Option<i32>,
    method: Option<String>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let precision = kwargs.get::<Option<i32>>("precision")?.or(precision);
    let method = kwargs.get::<Option<String>>("method")?.or(method);
    let scale = |x: f64, op: fn(f64) -> f64| {
        let p = 10f64.powi(precision.unwrap_or(0));
        Value::from(op(x * p) / p)
    };
    let number = || f64::try_from(value.clone());
    match method.as_deref() {
        None | Some("common") => f::round(value, precision),
        Some("ceil") => Ok(scale(number()?, f64::ceil)),
        Some("floor") => Ok(scale(number()?, f64::floor)),
        Some(_) => Err(failed("round", "method must be common, ceil or floor")),
    }
}

/// `value[attribute]`, a dotted path read part by part and a number read as an index, as Jinja2's
/// `make_attrgetter` reads it.
fn attribute_of(value: &Value, path: &str) -> Value {
    path.split('.').fold(value.clone(), |at, part| {
        let key = part
            .parse::<i64>()
            .map_or_else(|_| Value::from(part), Value::from);
        at.get_item(&key).unwrap_or(Value::UNDEFINED)
    })
}

/// `sum(iterable, attribute=None, start=0)`.
pub(super) fn sum(state: &State, values: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let attribute = kwargs.get::<Option<String>>("attribute")?;
    let start = kwargs.get::<Option<Value>>("start")?;
    kwargs.assert_all_used()?;
    let values = match &attribute {
        Some(path) => values
            .try_iter()?
            .map(|item| attribute_of(&item, path))
            .collect(),
        None => values,
    };
    let total = f::sum(state, values)?;
    match start {
        None => Ok(total),
        Some(start) if start.kind() != ValueKind::Number => Err(Error::new(
            ErrorKind::InvalidOperation,
            format!("can only sum numbers, got {}", start.kind()),
        )),
        Some(start) => f::sum(state, Value::from(vec![start, total])),
    }
}

/// `replace(s, old, new, count=None)`: Python's `str.replace`, where a negative count replaces
/// every match.
pub(super) fn replace(
    state: &mut State,
    value: StringInput<'_>,
    old: StringInput<'_>,
    new: StringInput<'_>,
    count: Option<i64>,
) -> Result<Value, Error> {
    match count {
        None => f::replace(state, value, old, new),
        Some(n) => {
            let (text, old, new) = (value.as_str(), old.as_str(), new.as_str());
            Ok(Value::from(match usize::try_from(n) {
                Ok(n) => text.replacen(old, new, n),
                Err(_) => text.replace(old, new),
            }))
        }
    }
}
