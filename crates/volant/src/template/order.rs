// SPDX-License-Identifier: GPL-3.0-or-later
//! Python's comparisons between template values, exact for every integer. The comparison
//! operators, the `min`, `max`, `sort` and `unique` filters and `list.sort()` all read them.
//!
//! minijinja orders two values by their kind before their value, and an integer wider than 128
//! bits is an object to it, so its own `<` puts a negative wide integer above every number.

use std::cmp::Ordering;

use minijinja::value::{Tuple, ValueKind};
use minijinja::{Environment, Error, ErrorKind, Value};

use super::bigint;
use super::clock::DateTime;
use super::pymethods::type_of;

/// The operators routed here: the operator, the global its instruction is compiled to call, and
/// the global a link of a chained comparison (`a < b < c`) calls, which hands back the result
/// and the right operand.
pub(super) const OPERATORS: [(&str, &str, &str); 6] = [
    ("==", "volant::eq", "volant::eq_keep"),
    ("!=", "volant::ne", "volant::ne_keep"),
    ("<", "volant::lt", "volant::lt_keep"),
    ("<=", "volant::le", "volant::le_keep"),
    (">", "volant::gt", "volant::gt_keep"),
    (">=", "volant::ge", "volant::ge_keep"),
];

/// Whether `name` is one of the globals of [`OPERATORS`], which no variable may shadow.
pub(super) fn is_key(name: &str) -> bool {
    OPERATORS
        .iter()
        .any(|(_, key, keep)| *key == name || *keep == name)
}

/// Installs the globals of [`OPERATORS`] on `env`.
pub(super) fn register(env: &mut Environment<'static>) {
    for (op, key, keep) in OPERATORS {
        env.add_function(key, move |a: Value, b: Value| {
            compare(op, &a, &b).map(Value::from)
        });
        // The VM unpacks the answer so that the result is on top and the right operand under it,
        // where the next link of the chain reads it.
        env.add_function(keep, move |a: Value, b: Value| {
            let result = compare(op, &a, &b)?;
            Ok::<_, Error>(Value::from(vec![Value::from(result), b]))
        });
    }
}

/// `a OP b` for one of the operators of [`OPERATORS`]. An undefined operand is an undefined read,
/// as the VM's own operators make it; a side that is `nan` makes every ordering false.
fn compare(op: &str, a: &Value, b: &Value) -> Result<bool, Error> {
    if a.is_undefined() || b.is_undefined() {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    Ok(match op {
        "==" => equal(a, b),
        "!=" => !equal(a, b),
        _ => match order(a, b, op)? {
            None => false,
            Some(o) => match op {
                "<" => o == Ordering::Less,
                "<=" => o != Ordering::Greater,
                ">" => o == Ordering::Greater,
                _ => o != Ordering::Less,
            },
        },
    })
}

/// `a == b`: numbers by value, exactly, a bool counting as 0 or 1, and anything else as
/// minijinja tells it.
pub(crate) fn equal(a: &Value, b: &Value) -> bool {
    match numeric(a, b) {
        Some(order) => order == Some(Ordering::Equal),
        None => a == b,
    }
}

/// How Python orders `a` against `b`, for `op`, which names the operator in the refusal of two
/// values it cannot order. `Ok(None)` when a side is `nan`.
pub(crate) fn order(a: &Value, b: &Value, op: &str) -> Result<Option<Ordering>, Error> {
    if a.is_undefined() || b.is_undefined() {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    if let Some(order) = numeric(a, b) {
        return Ok(order);
    }
    match (a.kind(), b.kind()) {
        (ValueKind::String, ValueKind::String) => Ok(Some(a.as_str().cmp(&b.as_str()))),
        // Two `datetime` values, which order themselves.
        _ if is_datetime(a) && is_datetime(b) => Ok(a.partial_cmp(b)),
        // Python compares the first items that differ, then the lengths; a tuple orders against
        // a tuple only, and a list against a list.
        (ValueKind::Seq, ValueKind::Seq) if a.is_tuple() == b.is_tuple() => {
            let (x, y) = (items(a)?, items(b)?);
            for (p, q) in x.iter().zip(&y) {
                if !equal(p, q) {
                    return order(p, q, op);
                }
            }
            Ok(Some(x.len().cmp(&y.len())))
        }
        _ => Err(Error::new(
            ErrorKind::InvalidOperation,
            format!(
                "'{op}' not supported between instances of '{}' and '{}'",
                type_of(a),
                type_of(b)
            ),
        )),
    }
}

fn is_datetime(value: &Value) -> bool {
    value.downcast_object_ref::<DateTime>().is_some()
}

/// The items of a list or a tuple.
fn items(value: &Value) -> Result<Vec<Value>, Error> {
    if let Some(tuple) = value.downcast_object_ref::<Tuple>() {
        return Ok(tuple.to_vec());
    }
    value.try_iter().map(Iterator::collect)
}

/// `a` against `b` when both are numbers: `None` when one is not, `Some(None)` against `nan`.
fn numeric(a: &Value, b: &Value) -> Option<Option<Ordering>> {
    if let Some(order) = bigint::order(a, b) {
        return Some(order);
    }
    Some(number(a)?.cmp_exact(number(b)?))
}

/// A number as `<` reads it: an integer stays one, so that a large integer is compared with a float
/// by value, as Python does, and not after rounding it to a float.
#[derive(Clone, Copy)]
enum Num {
    Int(i128),
    /// An integer past `i128::MAX`, which minijinja keeps as a `u128`: up to 2^128 - 1.
    Wide(u128),
    Float(f64),
}

fn number(v: &Value) -> Option<Num> {
    match v.kind() {
        ValueKind::Bool => Some(Num::Int(i128::from(v.is_true()))),
        ValueKind::Number if v.is_integer() => match i128::try_from(v.clone()) {
            Ok(n) => Some(Num::Int(n)),
            Err(_) => u128::try_from(v.clone()).ok().map(Num::Wide),
        },
        ValueKind::Number => f64::try_from(v.clone()).ok().map(Num::Float),
        _ => None,
    }
}

impl Num {
    /// `None` when a side is `nan`.
    fn cmp_exact(self, other: Num) -> Option<Ordering> {
        match (self, other) {
            (Num::Int(x), Num::Int(y)) => Some(x.cmp(&y)),
            (Num::Wide(x), Num::Wide(y)) => Some(x.cmp(&y)),
            // A wide integer is past every `i128`.
            (Num::Wide(_), Num::Int(_)) => Some(Ordering::Greater),
            (Num::Int(_), Num::Wide(_)) => Some(Ordering::Less),
            (Num::Wide(x), Num::Float(y)) => wide_against_float(x, y),
            (Num::Float(x), Num::Wide(y)) => wide_against_float(y, x).map(Ordering::reverse),
            (Num::Float(x), Num::Float(y)) => x.partial_cmp(&y),
            (Num::Int(x), Num::Float(y)) => int_against_float(x, y),
            (Num::Float(x), Num::Int(y)) => int_against_float(y, x).map(Ordering::reverse),
        }
    }
}

/// `x` against `y`, exactly.
fn int_against_float(x: i128, y: f64) -> Option<Ordering> {
    if y.is_nan() {
        return None;
    }
    // 2^127: a float at or past it is outside what an `i128` holds.
    let limit = 2f64.powi(127);
    if y >= limit {
        return Some(Ordering::Less);
    }
    if y < -limit {
        return Some(Ordering::Greater);
    }
    let whole = y.trunc();
    // `whole` is an integer value inside the range of `i128`, so the cast is exact.
    Some(x.cmp(&(whole as i128)).then_with(|| {
        // Equal wholes: the fraction of `y` decides.
        let fraction = y - whole;
        if fraction > 0.0 {
            Ordering::Less
        } else if fraction < 0.0 {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }))
}

/// `x`, at least 2^127, against `y`, exactly. A float of that size holds no fraction, so one
/// below 2^128 converts to a `u128` without loss.
fn wide_against_float(x: u128, y: f64) -> Option<Ordering> {
    if y.is_nan() {
        return None;
    }
    if y >= 2f64.powi(128) {
        return Some(Ordering::Less);
    }
    if y < 2f64.powi(127) {
        return Some(Ordering::Greater);
    }
    Some(x.cmp(&(y as u128)))
}

/// Python's sort of `items` by `<` on what `key` reads of each. As CPython does, a reverse sort
/// reverses the items, sorts them and reverses them again, which keeps equal items in their
/// order. A `nan` is never less nor greater than anything, so where it lands depends on the
/// algorithm: under 64 items this is CPython's (one run, then binary insertion), which places a
/// `nan` where Python does. A longer list holding one may come out in another order than
/// CPython's merges give.
pub(crate) fn sort<T>(
    mut items: Vec<T>,
    key: &dyn Fn(&T) -> &Value,
    reverse: bool,
) -> Result<Vec<T>, Error> {
    let less = |a: &T, b: &T| -> Result<bool, Error> {
        Ok(order(key(a), key(b), "<")? == Some(Ordering::Less))
    };
    if reverse {
        items.reverse();
    }
    let mut items = if items.len() < 64 {
        let ok = count_run(&mut items, &less)?;
        binary_insertion(&mut items, ok, &less)?;
        items
    } else {
        merge_sort(items, &less)?
    };
    if reverse {
        items.reverse();
    }
    Ok(items)
}

/// CPython's `count_run`: the length of the run that starts the list, which it turns ascending.
/// A run is non-descending, or descending where equal items keep their order.
fn count_run<T>(a: &mut [T], less: &dyn Fn(&T, &T) -> Result<bool, Error>) -> Result<usize, Error> {
    let n = a.len();
    if n < 2 {
        return Ok(n);
    }
    let mut k = 1;
    while k < n && !less(&a[k], &a[k - 1])? {
        k += 1;
    }
    if k == n {
        return Ok(k);
    }
    if k > 1 {
        if less(&a[0], &a[k - 1])? {
            return Ok(k);
        }
        a[..k].reverse();
    }
    k += 1;
    // The last `equal + 1` items seen are equal: they are reversed now so that the reversal of
    // the whole run puts them back in their order.
    let mut equal = 0;
    while k < n {
        if less(&a[k], &a[k - 1])? {
            if equal > 0 {
                a[k - equal - 1..k].reverse();
                equal = 0;
            }
        } else if less(&a[k - 1], &a[k])? {
            break;
        } else {
            equal += 1;
        }
        k += 1;
    }
    if equal > 0 {
        a[k - equal - 1..k].reverse();
    }
    a[..k].reverse();
    while k < n && !less(&a[k], &a[k - 1])? {
        k += 1;
    }
    Ok(k)
}

/// CPython's `binarysort`: each item after the first `sorted` goes after every earlier item it
/// is not less than.
fn binary_insertion<T>(
    a: &mut [T],
    sorted: usize,
    less: &dyn Fn(&T, &T) -> Result<bool, Error>,
) -> Result<(), Error> {
    for i in sorted.max(1)..a.len() {
        let (mut low, mut high) = (0, i);
        while low < high {
            let mid = (low + high) / 2;
            if less(&a[i], &a[mid])? {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        a[low..=i].rotate_right(1);
    }
    Ok(())
}

/// A stable sort by a comparison that can fail. `slice::sort_by` may panic when the comparison
/// is not a total order, and a list of mixed types is not one.
fn merge_sort<T>(
    items: Vec<T>,
    less: &dyn Fn(&T, &T) -> Result<bool, Error>,
) -> Result<Vec<T>, Error> {
    if items.len() < 2 {
        return Ok(items);
    }
    let mut right = items;
    let left = right.drain(..right.len() / 2).collect::<Vec<_>>();
    let (left, right) = (merge_sort(left, less)?, merge_sort(right, less)?);
    let mut out = Vec::with_capacity(left.len() + right.len());
    let (mut l, mut r) = (left.into_iter().peekable(), right.into_iter().peekable());
    while let (Some(a), Some(b)) = (l.peek(), r.peek()) {
        // The right item goes first only when it is strictly smaller, which keeps equal items in
        // their order.
        let next = if less(b, a)? { &mut r } else { &mut l };
        out.extend(next.next());
    }
    out.extend(l);
    out.extend(r);
    Ok(out)
}

/// What `unique` tells items apart by: a bool as its integer and a float with no fraction as the
/// integer it is, so that minijinja's own equality finds `1`, `true` and `1.0` the same, as
/// Python's set does. `None` for `nan`, which equals nothing: Python's set only merges a `nan`
/// with the very same object, which a template value cannot tell, so every `nan` is kept.
pub(crate) fn unique_key(value: &Value) -> Option<Value> {
    Some(match value.kind() {
        ValueKind::Bool => Value::from(i64::from(value.is_true())),
        ValueKind::Number if !value.is_integer() => match f64::try_from(value.clone()) {
            Ok(f) if f.is_nan() => return None,
            Ok(f) if f.is_finite() && f.fract() == 0.0 => {
                bigint::integer(&format!("{f:.0}")).unwrap_or_else(|| value.clone())
            }
            _ => value.clone(),
        },
        _ => value.clone(),
    })
}

#[cfg(test)]
mod tests;
