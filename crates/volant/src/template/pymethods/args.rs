// SPDX-License-Identifier: GPL-3.0-or-later
//! The arguments of a Python method call, and the errors Python raises for the wrong ones.
//!
//! The wording follows CPython 3.13, which ansible-core 2.19 runs on, so that a template that
//! fails there fails here with the same sentence.

use minijinja::value::ValueKind;
use minijinja::{Error, ErrorKind, Value};

/// The error a method raises: Python's own message, which the reference prints without the name
/// of the exception class.
pub(super) fn raise(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, message.into())
}

/// Python's name for the type of a value, as its error messages write it.
pub(crate) fn type_of(value: &Value) -> &'static str {
    if crate::template::bigint::is_big(value) {
        return "int";
    }
    match value.kind() {
        ValueKind::Undefined => "UndefinedType",
        ValueKind::None => "NoneType",
        ValueKind::Bool => "bool",
        ValueKind::Number if value.is_integer() => "int",
        ValueKind::Number => "float",
        ValueKind::String => "str",
        ValueKind::Bytes => "bytes",
        ValueKind::Seq if value.is_tuple() => "tuple",
        ValueKind::Seq => "list",
        ValueKind::Map => "dict",
        ValueKind::Iterable => "generator",
        _ => "object",
    }
}

/// A call split into its positional arguments and its keyword arguments, in the order written.
pub(super) struct Call<'a> {
    pub(super) pos: &'a [Value],
    pub(super) kw: Vec<(String, Value)>,
    /// The type the method belongs to, as Python words it in an error: `str`, `list`, `dict`.
    owner: &'static str,
}

impl<'a> Call<'a> {
    pub(super) fn new(args: &'a [Value]) -> Self {
        Self::of("str", args)
    }

    pub(super) fn of(owner: &'static str, args: &'a [Value]) -> Self {
        let (pos, kw) = match args.split_last() {
            Some((last, rest)) if last.is_kwargs() => {
                let pairs = last
                    .try_iter()
                    .map(|keys| {
                        keys.filter_map(|k| {
                            let value = last.get_item(&k).ok()?;
                            Some((k.to_string(), value))
                        })
                        .collect()
                    })
                    .unwrap_or_default();
                (rest, pairs)
            }
            _ => (args, Vec::new()),
        };
        Self { pos, kw, owner }
    }

    /// Methods that take no argument: `str.upper() takes no arguments (1 given)`.
    pub(super) fn none(&self, name: &str) -> Result<(), Error> {
        self.no_keywords(name)?;
        match self.pos.len() {
            0 => Ok(()),
            n => Err(raise(format!(
                "str.{name}() takes no arguments ({n} given)"
            ))),
        }
    }

    /// Methods that take one argument: `str.join() takes exactly one argument (2 given)`.
    pub(super) fn one(&self, name: &str) -> Result<&'a Value, Error> {
        self.no_keywords(name)?;
        match self.pos {
            [one] => Ok(one),
            other => Err(raise(format!(
                "{}.{name}() takes exactly one argument ({} given)",
                self.owner,
                other.len()
            ))),
        }
    }

    /// Methods that take between `min` and `max` positional arguments and no keyword.
    pub(super) fn between(&self, name: &str, min: usize, max: usize) -> Result<&'a [Value], Error> {
        self.no_keywords(name)?;
        let given = self.pos.len();
        let plural = |n: usize| if n == 1 { "argument" } else { "arguments" };
        if min == max && given != min {
            return Err(raise(format!(
                "{name} expected {min} {}, got {given}",
                plural(min)
            )));
        }
        if given < min {
            return Err(raise(format!(
                "{name} expected at least {min} {}, got {given}",
                plural(min)
            )));
        }
        if given > max {
            return Err(raise(format!(
                "{name} expected at most {max} {}, got {given}",
                plural(max)
            )));
        }
        Ok(self.pos)
    }

    /// Methods whose arguments can be named: one slot for each name, filled from the positional
    /// arguments first and then from the keywords. `limit` is the most positional arguments.
    pub(super) fn named(&self, name: &str, params: &[&str]) -> Result<Vec<Option<&Value>>, Error> {
        let limit = params.len();
        let plural = if limit == 1 { "argument" } else { "arguments" };
        if self.pos.len() > limit {
            return Err(raise(format!(
                "{name}() takes at most {limit} {plural} ({} given)",
                self.pos.len() + self.kw.len()
            )));
        }
        let mut slots: Vec<Option<&Value>> = vec![None; limit];
        for (slot, value) in slots.iter_mut().zip(self.pos) {
            *slot = Some(value);
        }
        for (key, value) in &self.kw {
            let Some(at) = params.iter().position(|p| p == key) else {
                return Err(raise(format!(
                    "{name}() got an unexpected keyword argument '{key}'"
                )));
            };
            if slots[at].is_some() {
                return Err(if at < self.pos.len() {
                    raise(format!(
                        "argument for {name}() given by name ('{key}') and position ({})",
                        at + 1
                    ))
                } else {
                    raise(format!("{name}() got multiple values for argument '{key}'"))
                });
            }
            slots[at] = Some(value);
        }
        Ok(slots)
    }

    fn no_keywords(&self, name: &str) -> Result<(), Error> {
        if self.kw.is_empty() {
            Ok(())
        } else {
            Err(raise(format!(
                "{}.{name}() takes no keyword arguments",
                self.owner
            )))
        }
    }
}

/// An argument read as an integer, the way `operator.index` reads it: a bool counts as 0 or 1.
pub(super) fn index(value: &Value) -> Result<i64, Error> {
    match value.kind() {
        ValueKind::Bool => Ok(i64::from(value.is_true())),
        ValueKind::Number if value.is_integer() => value
            .as_i64()
            .ok_or_else(|| raise("Python int too large to convert to C ssize_t")),
        _ => Err(raise(format!(
            "'{}' object cannot be interpreted as an integer",
            type_of(value)
        ))),
    }
}

/// A `start` or `end` argument: an integer, or none for "not given".
pub(super) fn bound(value: Option<&Value>) -> Result<Option<i64>, Error> {
    match value {
        None => Ok(None),
        Some(v) if v.is_none() => Ok(None),
        Some(v) if matches!(v.kind(), ValueKind::Bool) => Ok(Some(i64::from(v.is_true()))),
        // An integer past what an index holds stands for the end it lies beyond.
        Some(v) if matches!(v.kind(), ValueKind::Number) && v.is_integer() => {
            Ok(Some(i128::try_from(v.clone()).map_or(i64::MAX, |n| {
                i64::try_from(n).unwrap_or(if n < 0 { i64::MIN } else { i64::MAX })
            })))
        }
        Some(_) => Err(raise(
            "slice indices must be integers or None or have an __index__ method",
        )),
    }
}

/// An argument that must be a string: `find() argument 1 must be str, not int`.
pub(super) fn text<'v>(value: &'v Value, what: &str) -> Result<&'v str, Error> {
    value
        .as_str()
        .ok_or_else(|| raise(format!("{what} must be str, not {}", type_of(value))))
}

/// Python's `str.isspace` set: Unicode white space and the four information separators.
pub(super) fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// A `start` and `end` pair made into a range of character positions of a text `len` long, as
/// CPython's `ADJUST_INDICES` does. `start` can be larger than `len`.
pub(super) fn clamp(len: usize, start: Option<i64>, end: Option<i64>) -> (usize, usize) {
    let len_i = i64::try_from(len).unwrap_or(i64::MAX);
    let fix = |v: i64| if v < 0 { (v + len_i).max(0) } else { v };
    let end = end.map_or(len_i, |e| fix(e).min(len_i));
    let start = start.map_or(0, fix);
    (
        usize::try_from(start).unwrap_or(usize::MAX),
        usize::try_from(end).unwrap_or(0),
    )
}

/// Every item of an iterable argument, as `list(x)` reads it: a list, the characters of a text,
/// the keys of a mapping.
pub(super) fn iterate(value: &Value) -> Result<Vec<Value>, Error> {
    match value.kind() {
        ValueKind::Seq | ValueKind::Map | ValueKind::Iterable | ValueKind::String => {
            Ok(value.try_iter()?.collect())
        }
        _ => Err(raise(format!(
            "'{}' object is not iterable",
            type_of(value)
        ))),
    }
}
