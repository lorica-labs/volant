// SPDX-License-Identifier: GPL-3.0-or-later
//! The methods of `bytes` beside `decode` and `hex`.
//!
//! The ones Python runs on ASCII alone, the case changes, the predicates and the splits on
//! whitespace, are written here. The others are the `str` methods of the same name run on the
//! bytes read one character per byte, their arguments read the same way and their answer written
//! back, which gives the same bytes as long as the method only moves, compares and copies them.

use minijinja::value::Tuple;
use minijinja::{Error, Value};

use super::super::args::{Call, raise, type_of};
use super::{Bytes, of};

/// What Python's bytes methods count as whitespace.
const WHITESPACE: &[u8] = b" \t\n\r\x0b\x0c";

/// The methods run as the `str` method of the same name, see [`through_str`].
pub(in crate::template::pymethods) const THROUGH_STR: [&str; 24] = [
    "center",
    "count",
    "endswith",
    "expandtabs",
    "find",
    "index",
    "istitle",
    "join",
    "ljust",
    "lstrip",
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
    "startswith",
    "strip",
    "zfill",
];

/// A method written here, or `None` for the caller to try [`through_str`].
pub(super) fn call(me: &Bytes, name: &str, args: &[Value]) -> Option<Result<Value, Error>> {
    let none = |args: &[Value]| -> Result<(), Error> {
        if args.is_empty() {
            Ok(())
        } else {
            Err(raise(format!(
                "bytes.{name}() takes no arguments ({} given)",
                args.len()
            )))
        }
    };
    let b = &me.0;
    Some(match name {
        "upper" | "lower" | "swapcase" | "capitalize" | "title" => {
            none(args).map(|()| Bytes::value(cased(b, name)))
        }
        "isalnum" | "isalpha" | "isdigit" | "isspace" | "isascii" | "islower" | "isupper" => {
            none(args).map(|()| Value::from(holds(b, name)))
        }
        "splitlines" => splitlines(b, args),
        "split" | "rsplit" | "strip" | "lstrip" | "rstrip" => {
            let call = Call::of("bytes", args);
            let names: &[&str] = if name.ends_with("split") {
                &["sep", "maxsplit"]
            } else {
                &["chars"]
            };
            let slots = match call.named(name, names) {
                Ok(slots) => slots,
                Err(e) => return Some(Err(e)),
            };
            if slots[0].is_some_and(|v| !v.is_none()) {
                return None;
            }
            if name.ends_with("split") {
                let most = slots
                    .get(1)
                    .copied()
                    .flatten()
                    .map_or(Some(-1), Value::as_i64);
                let Some(most) = most else {
                    return Some(Err(raise("'maxsplit' must be an integer")));
                };
                Ok(Value::from(
                    split_whitespace(b, most, name == "rsplit")
                        .into_iter()
                        .map(Bytes::value)
                        .collect::<Vec<_>>(),
                ))
            } else {
                Ok(Bytes::value(strip(b, name)))
            }
        }
        _ => return None,
    })
}

fn cased(b: &[u8], name: &str) -> Vec<u8> {
    match name {
        "upper" => b.to_ascii_uppercase(),
        "lower" => b.to_ascii_lowercase(),
        "swapcase" => b
            .iter()
            .map(|c| {
                if c.is_ascii_uppercase() {
                    c.to_ascii_lowercase()
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect(),
        "capitalize" => b
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect(),
        // `title`: a letter after a letter is lower case, any other letter upper case.
        _ => {
            let mut after_letter = false;
            b.iter()
                .map(|c| {
                    let out = if after_letter {
                        c.to_ascii_lowercase()
                    } else {
                        c.to_ascii_uppercase()
                    };
                    after_letter = c.is_ascii_alphabetic();
                    out
                })
                .collect()
        }
    }
}

fn holds(b: &[u8], name: &str) -> bool {
    let all = |test: fn(&u8) -> bool| !b.is_empty() && b.iter().all(test);
    match name {
        "isalnum" => all(u8::is_ascii_alphanumeric),
        "isalpha" => all(u8::is_ascii_alphabetic),
        "isdigit" => all(u8::is_ascii_digit),
        "isspace" => all(|c| WHITESPACE.contains(c)),
        "isascii" => b.is_ascii(),
        "islower" => b.iter().any(u8::is_ascii_lowercase) && !b.iter().any(u8::is_ascii_uppercase),
        _ => b.iter().any(u8::is_ascii_uppercase) && !b.iter().any(u8::is_ascii_lowercase),
    }
}

fn strip(b: &[u8], name: &str) -> Vec<u8> {
    let space = |c: &u8| WHITESPACE.contains(c);
    let start = if name == "rstrip" {
        0
    } else {
        b.iter().position(|c| !space(c)).unwrap_or(b.len())
    };
    let end = if name == "lstrip" {
        b.len()
    } else {
        b.iter().rposition(|c| !space(c)).map_or(start, |at| at + 1)
    };
    b[start..end.max(start)].to_vec()
}

/// `split()` and `rsplit()` with no separator: runs of whitespace split, at most `most` times,
/// from the right for `rsplit`; what is left after the last split keeps its own whitespace.
fn split_whitespace(b: &[u8], most: i64, right: bool) -> Vec<Vec<u8>> {
    let mut b = b.to_vec();
    if right {
        b.reverse();
    }
    let space = |c: &u8| WHITESPACE.contains(c);
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        while at < b.len() && space(&b[at]) {
            at += 1;
        }
        if at == b.len() {
            break;
        }
        if most >= 0 && i64::try_from(out.len()).unwrap_or(i64::MAX) == most {
            let mut rest = b[at..].to_vec();
            if right {
                rest.reverse();
            }
            out.push(rest);
            break;
        }
        let start = at;
        while at < b.len() && !space(&b[at]) {
            at += 1;
        }
        let mut word = b[start..at].to_vec();
        if right {
            word.reverse();
        }
        out.push(word);
    }
    if right {
        out.reverse();
    }
    out
}

/// `splitlines(keepends=False)`: bytes end a line at `\n`, `\r` and `\r\n` only.
fn splitlines(b: &[u8], args: &[Value]) -> Result<Value, Error> {
    let call = Call::of("bytes", args);
    let slots = call.named("splitlines", &["keepends"])?;
    let keep = slots[0].is_some_and(Value::is_true);
    let mut out = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < b.len() {
        let end = match b[at] {
            b'\r' if b.get(at + 1) == Some(&b'\n') => at + 2,
            b'\r' | b'\n' => at + 1,
            _ => {
                at += 1;
                continue;
            }
        };
        out.push(Bytes::value(b[start..if keep { end } else { at }].to_vec()));
        start = end;
        at = end;
    }
    if start < b.len() {
        out.push(Bytes::value(b[start..].to_vec()));
    }
    Ok(Value::from(out))
}

/// The text of `b`, one character per byte.
fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| char::from(c)).collect()
}

/// An argument of the bytes method `method` read as the `str` method's: bytes as their
/// characters, and a list or tuple item by item. A text is refused as Python refuses it.
fn as_text(method: &str, value: &Value) -> Result<Value, Error> {
    if let Some(bytes) = of(value) {
        return Ok(Value::from(latin1(&bytes.0)));
    }
    if value.as_str().is_some() {
        // The searches take an integer too, and say so.
        return Err(raise(
            if matches!(method, "count" | "find" | "index" | "rfind" | "rindex") {
                "argument should be integer or bytes-like object, not 'str'"
            } else {
                "a bytes-like object is required, not 'str'"
            },
        ));
    }
    if matches!(type_of(value), "list" | "tuple") {
        let items = value
            .try_iter()?
            .map(|item| as_text(method, &item))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(if type_of(value) == "tuple" {
            Value::from(Tuple::from(items))
        } else {
            Value::from(items)
        });
    }
    Ok(value.clone())
}

/// The answer of the `str` method written back as bytes: a text as its bytes, a list or tuple
/// item by item.
fn as_bytes(value: Value) -> Value {
    if let Some(text) = value.as_str() {
        return Bytes::value(
            text.chars()
                .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                .collect(),
        );
    }
    if matches!(type_of(&value), "list" | "tuple") {
        let items: Vec<Value> = value
            .try_iter()
            .map(|items| items.map(as_bytes).collect())
            .unwrap_or_default();
        return if type_of(&value) == "tuple" {
            Value::from(Tuple::from(items))
        } else {
            Value::from(items)
        };
    }
    value
}

/// `me.method(args)` as the `str` method of that name answers it, which `run` calls.
pub(in crate::template::pymethods) fn through_str(
    me: &Bytes,
    method: &str,
    args: &[Value],
    run: impl FnOnce(&Value, &[Value]) -> Result<Value, Error>,
) -> Result<Value, Error> {
    let args = args
        .iter()
        .map(|arg| as_text(method, arg))
        .collect::<Result<Vec<_>, _>>()?;
    run(&Value::from(latin1(&me.0)), &args).map(as_bytes)
}
