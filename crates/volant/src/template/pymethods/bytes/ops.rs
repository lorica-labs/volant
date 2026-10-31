// SPDX-License-Identifier: GPL-3.0-or-later
//! `+` and slicing on bytes, routed here in a template that calls `encode`, and the bytes a loop
//! keeps in its items.

use std::cell::Cell;

use minijinja::{Error, State, Value, context};

use super::{Bytes, of};
use crate::template::pymethods::args::{raise, type_of};

/// `left + right` when either is bytes: bytes joined to bytes, or Python's refusal. `None` when
/// neither is, for the operator to take its usual course.
pub(crate) fn add(left: &Value, right: &Value) -> Option<Result<Value, Error>> {
    Some(match (of(left), of(right)) {
        (Some(a), Some(b)) => Ok(Bytes::value([a.0.as_slice(), b.0.as_slice()].concat())),
        (Some(_), None) => Err(raise(format!("can't concat {} to bytes", type_of(right)))),
        (None, Some(_)) if matches!(type_of(left), "str" | "list") => Err(raise(format!(
            "can only concatenate {0} (not \"bytes\") to {0}",
            type_of(left)
        ))),
        (None, Some(_)) => Err(raise(format!(
            "unsupported operand type(s) for +: '{}' and 'bytes'",
            type_of(left)
        ))),
        (None, None) => return None,
    })
}

/// `value[start:stop:step]`: bytes for bytes, as Python slices them, and minijinja's own slice
/// for anything else.
pub(crate) fn slice(
    state: &State,
    value: Value,
    start: Value,
    stop: Value,
    step: Value,
) -> Result<Value, Error> {
    let Some(bytes) = of(&value) else {
        let expression = state.env().compile_expression("v[a:b:c]")?;
        return expression.eval(context! { v => value, a => start, b => stop, c => step });
    };
    let bound = |v: &Value| -> Result<Option<i64>, Error> {
        if v.is_none() || v.is_undefined() {
            return Ok(None);
        }
        v.as_i64().map(Some).ok_or_else(|| {
            raise("slice indices must be integers or None or have an __index__ method")
        })
    };
    let step = bound(&step)?.unwrap_or(1);
    if step == 0 {
        return Err(raise("slice step cannot be zero"));
    }
    let len = i64::try_from(bytes.0.len()).unwrap_or(i64::MAX);
    // `PySlice_AdjustIndices`: a negative bound counts from the end, then each is clamped.
    let adjust = |at: Option<i64>, default: i64| match at {
        None => default,
        Some(at) => {
            let at = if at < 0 { at + len } else { at };
            if step < 0 {
                at.clamp(-1, len - 1)
            } else {
                at.clamp(0, len)
            }
        }
    };
    let (from, to) = if step > 0 {
        (adjust(bound(&start)?, 0), adjust(bound(&stop)?, len))
    } else {
        (adjust(bound(&start)?, len - 1), adjust(bound(&stop)?, -1))
    };
    let mut out = Vec::new();
    let mut at = from;
    while (step > 0 && at < to) || (step < 0 && at > to) {
        out.extend(usize::try_from(at).ok().and_then(|i| bytes.0.get(i)));
        at += step;
    }
    Ok(Bytes::value(out))
}

thread_local! {
    /// Set while a loop's list renders: a bytes item stays bytes, as the reference's loop item
    /// does, where a result reads bytes as their text.
    static KEEP: Cell<bool> = const { Cell::new(false) };
}

/// Runs `render` with bytes kept as such in what it hands back, see [`kept`].
pub(crate) fn keeping<T>(render: impl FnOnce() -> T) -> T {
    let before = KEEP.replace(true);
    let out = render();
    KEEP.set(before);
    out
}

/// What a bytes value becomes in a result: while [`keeping`], the wrapper a `!!binary` value
/// loads as, which reads back as the same bytes; otherwise `None`, and it is read as its text.
pub(super) fn kept(bytes: &Bytes) -> Option<Value> {
    KEEP.get().then(|| {
        Value::from(crate::yaml::binary_data(
            &volant_protocol::encoding::b64_encode(&bytes.0),
        ))
    })
}
