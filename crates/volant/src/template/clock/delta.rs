// SPDX-License-Identifier: GPL-3.0-or-later
//! Python's `timedelta`, `+` and `-` between `datetime` and `timedelta` values, and `replace`.
//!
//! minijinja has no operator a value of its own can take part in, so a template that calls
//! `now()` has its `+` and `-` routed here (see `route_calls`). Anything that is neither a
//! `datetime` nor a `timedelta` goes back to minijinja's own operator.

use std::fmt;
use std::sync::Arc;

use minijinja::value::{Object, ObjectRepr, ValueKind};
use minijinja::{Error, State, Value, context};

use super::{DateTime, days_from_civil, no_arguments, python_raise, raise};
use crate::template::globals::{Callee, bind, python_type};

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// A duration, kept the way Python normalises it: whole days, then seconds below a day, then
/// microseconds below a second, only `days` negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::template) struct TimeDelta {
    days: i64,
    seconds: i64,
    microseconds: i64,
}

impl TimeDelta {
    fn from_micros(total: i64) -> Self {
        let days = total.div_euclid(MICROS_PER_DAY);
        let rest = total.rem_euclid(MICROS_PER_DAY);
        Self {
            days,
            seconds: rest / 1_000_000,
            microseconds: rest % 1_000_000,
        }
    }

    fn micros(self) -> i64 {
        self.days * MICROS_PER_DAY + self.seconds * 1_000_000 + self.microseconds
    }

    /// Python's `str`: `-1 day, 23:59:59.999995`.
    pub(in crate::template) fn python_str(&self) -> String {
        let clock = format!(
            "{}:{:02}:{:02}",
            self.seconds / 3600,
            self.seconds % 3600 / 60,
            self.seconds % 60
        );
        let clock = if self.microseconds == 0 {
            clock
        } else {
            format!("{clock}.{:06}", self.microseconds)
        };
        match self.days {
            0 => clock,
            1 | -1 => format!("{} day, {clock}", self.days),
            days => format!("{days} days, {clock}"),
        }
    }
}

impl TimeDelta {
    /// Python's `repr`, which names the fields that are not zero.
    fn python_repr(&self) -> String {
        let fields: Vec<String> = [
            ("days", self.days),
            ("seconds", self.seconds),
            ("microseconds", self.microseconds),
        ]
        .iter()
        .filter(|(_, n)| *n != 0)
        .map(|(name, n)| format!("{name}={n}"))
        .collect();
        if fields.is_empty() {
            "datetime.timedelta(0)".into()
        } else {
            format!("datetime.timedelta({})", fields.join(", "))
        }
    }
}

/// Python's `repr` of a `datetime`: its fields, the second and microsecond only when set.
fn datetime_repr(at: &DateTime) -> String {
    let mut fields = vec![
        at.year.to_string(),
        at.month.to_string(),
        at.day.to_string(),
        at.hour.to_string(),
        at.minute.to_string(),
    ];
    if at.second != 0 || at.microsecond != 0 {
        fields.push(at.second.to_string());
    }
    if at.microsecond != 0 {
        fields.push(at.microsecond.to_string());
    }
    format!("datetime.datetime({})", fields.join(", "))
}

fn holds_clock(value: &Value) -> bool {
    if value.downcast_object_ref::<DateTime>().is_some()
        || value.downcast_object_ref::<TimeDelta>().is_some()
    {
        return true;
    }
    matches!(value.kind(), ValueKind::Seq | ValueKind::Map)
        && value.try_iter().is_ok_and(|mut items| {
            items.any(|item| {
                holds_clock(&item)
                    || (value.kind() == ValueKind::Map
                        && value.get_item(&item).is_ok_and(|v| holds_clock(&v)))
            })
        })
}

fn repr_of(value: &Value) -> String {
    if let Some(at) = value.downcast_object_ref::<DateTime>() {
        return datetime_repr(at);
    }
    if let Some(span) = value.downcast_object_ref::<TimeDelta>() {
        return span.python_repr();
    }
    let items = || value.try_iter().into_iter().flatten();
    match value.kind() {
        ValueKind::Seq => format!(
            "[{}]",
            items().map(|v| repr_of(&v)).collect::<Vec<_>>().join(", ")
        ),
        ValueKind::Map => format!(
            "{{{}}}",
            items()
                .map(|k| format!(
                    "{}: {}",
                    repr_of(&k),
                    repr_of(&value.get_item(&k).unwrap_or_default())
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => crate::template::bigint::to_json(value)
            .map(|json| crate::playbook::python_repr(&json))
            .unwrap_or_default(),
    }
}

/// Python's `repr` of a list or mapping that holds a `datetime` or a `timedelta`, which JSON
/// would turn into text.
pub(in crate::template) fn container_repr(value: &Value) -> Option<String> {
    (matches!(value.kind(), ValueKind::Seq | ValueKind::Map) && holds_clock(value))
        .then(|| repr_of(value))
}

impl Object for TimeDelta {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.python_str())
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        Some(match key.as_str()? {
            "days" => Value::from(self.days),
            "seconds" => Value::from(self.seconds),
            "microseconds" => Value::from(self.microseconds),
            _ => return None,
        })
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        match method {
            "total_seconds" => {
                no_arguments(method, args)?;
                #[expect(clippy::cast_precision_loss, reason = "Python's float of the count")]
                Ok(Value::from(self.micros() as f64 / 1e6))
            }
            _ => Err(Error::from(minijinja::ErrorKind::UnknownMethod)),
        }
    }
}

/// The microseconds since the epoch the fields of `at` stand for, read as UTC.
fn micros_of(at: &DateTime) -> i64 {
    at.utc_seconds() * 1_000_000 + i64::from(at.microsecond)
}

fn at_micros(total: i64) -> DateTime {
    let micros = u32::try_from(total.rem_euclid(1_000_000)).unwrap_or(0);
    DateTime::from_utc(total.div_euclid(1_000_000), micros)
}

/// Python's name for an operand, as `unsupported operand type(s)` writes it.
fn operand_type(value: &Value) -> &'static str {
    if value.downcast_object_ref::<DateTime>().is_some() {
        "datetime.datetime"
    } else if value.downcast_object_ref::<TimeDelta>().is_some() {
        "datetime.timedelta"
    } else {
        crate::template::pymethods::type_of(value)
    }
}

/// `left - right`, or `left + right` when `add`.
fn arith(state: &State, left: &Value, right: &Value, add: bool) -> Result<Value, Error> {
    let at = |v: &Value| v.downcast_object_ref::<DateTime>().map(micros_of);
    let span = |v: &Value| v.downcast_object_ref::<TimeDelta>().map(|d| d.micros());
    let result = match (at(left), span(left), at(right), span(right)) {
        (Some(a), _, Some(b), _) if !add => Some(Value::from_object(TimeDelta::from_micros(a - b))),
        (Some(a), _, _, Some(d)) => Some(Value::from_object(at_micros(if add {
            a + d
        } else {
            a - d
        }))),
        (_, Some(d), Some(a), _) if add => Some(Value::from_object(at_micros(a + d))),
        (_, Some(a), _, Some(b)) => Some(Value::from_object(TimeDelta::from_micros(if add {
            a + b
        } else {
            a - b
        }))),
        _ => None,
    };
    if let Some(result) = result {
        return Ok(result);
    }
    let ours = |v: &Value| at(v).is_some() || span(v).is_some();
    let symbol = if add { "+" } else { "-" };
    if ours(left) || ours(right) {
        return Err(python_raise(format!(
            "unsupported operand type(s) for {symbol}: '{}' and '{}'",
            operand_type(left),
            operand_type(right)
        )));
    }
    // Neither operand is ours: minijinja's own operator, through an expression of the same
    // environment, which this rewrite never touches.
    let expression = state
        .env()
        .compile_expression(if add { "l + r" } else { "l - r" })?;
    expression.eval(context! { l => left, r => right })
}

/// The routed `+`.
pub(in crate::template) fn add(state: &State, left: Value, right: Value) -> Result<Value, Error> {
    arith(state, &left, &right, true)
}

/// The routed `-`.
pub(in crate::template) fn sub(state: &State, left: Value, right: Value) -> Result<Value, Error> {
    arith(state, &left, &right, false)
}

impl DateTime {
    /// `replace(year=, month=, day=, hour=, minute=, second=, microsecond=)`, checked as Python
    /// checks a new `datetime`.
    pub(super) fn replace_method(&self, args: &[Value]) -> Result<Value, Error> {
        const NAMES: [&str; 7] = [
            "year",
            "month",
            "day",
            "hour",
            "minute",
            "second",
            "microsecond",
        ];
        let given = bind(Callee::Native, "replace", &NAMES, args)?;
        let mut fields = [
            self.year,
            i64::from(self.month),
            i64::from(self.day),
            i64::from(self.hour),
            i64::from(self.minute),
            i64::from(self.second),
            i64::from(self.microsecond),
        ];
        for (field, value) in fields.iter_mut().zip(given) {
            if let Some(value) = value {
                *field = i64::try_from(value.clone()).map_err(|_| {
                    raise(format!(
                        "'{}' object cannot be interpreted as an integer",
                        python_type(&value)
                    ))
                })?;
            }
        }
        let [year, month, day, hour, minute, second, microsecond] = fields;
        let check = |ok: bool, what: &str| {
            if ok {
                Ok(())
            } else {
                Err(python_raise(what.into()))
            }
        };
        check((1..=9999).contains(&year), "year must be in 1..9999")?;
        check((1..=12).contains(&month), "month must be in 1..12")?;
        let next = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        let in_month = days_from_civil(next.0, u32::try_from(next.1).unwrap_or(1), 1)
            - days_from_civil(year, u32::try_from(month).unwrap_or(1), 1);
        check(
            (1..=in_month).contains(&day),
            "day is out of range for month",
        )?;
        check((0..24).contains(&hour), "hour must be in 0..23")?;
        check((0..60).contains(&minute), "minute must be in 0..59")?;
        check((0..60).contains(&second), "second must be in 0..59")?;
        check(
            (0..1_000_000).contains(&microsecond),
            "microsecond must be in 0..999999",
        )?;
        let field = |n: i64| u32::try_from(n).unwrap_or(0);
        Ok(Value::from_object(Self {
            year,
            month: field(month),
            day: field(day),
            hour: field(hour),
            minute: field(minute),
            second: field(second),
            microsecond: field(microsecond),
        }))
    }
}
