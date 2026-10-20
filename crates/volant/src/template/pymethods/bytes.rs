// SPDX-License-Identifier: GPL-3.0-or-later
//! Python's `bytes`, as far as a template meets it: what `str.encode` makes.
//!
//! A [`Bytes`] value counts, indexes and iterates as a sequence of integers, prints as Python's
//! `str(bytes)` does (`b'abc'`), and has the methods `decode` and `hex`. A whole result that is
//! bytes becomes its text, as in ansible-core, which reads a bytes result as UTF-8. The
//! filters that read bytes as text (`b64encode`, `b64decode`, `quote`, the regex filters) do so
//! here too. What nothing here runs fails by name and does not pass for a list of integers.

use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use minijinja::value::{Enumerator, Object, ObjectRepr, ValueKind};
use minijinja::{Error, Value};

use super::args::{Call, index, raise, type_of};
use super::codec;
use super::limits::check;

/// Set when a template first made a bytes value. A result is walked for bytes only after that,
/// so a run that never calls `encode` pays nothing.
static MADE: AtomicBool = AtomicBool::new(false);

/// A bytes value.
#[derive(Debug)]
pub(crate) struct Bytes(pub(crate) Vec<u8>);

impl Bytes {
    pub(super) fn value(bytes: Vec<u8>) -> Value {
        MADE.store(true, Ordering::Relaxed);
        Value::from_object(Bytes(bytes))
    }

    /// Python's `repr`: single quotes unless the bytes hold a `'` and no `"`.
    pub(crate) fn python_repr(&self) -> String {
        let quote = if self.0.contains(&b'\'') && !self.0.contains(&b'"') {
            '"'
        } else {
            '\''
        };
        let mut out = format!("b{quote}");
        for &b in &self.0 {
            match b {
                b'\\' => out.push_str("\\\\"),
                b'\t' => out.push_str("\\t"),
                b'\n' => out.push_str("\\n"),
                b'\r' => out.push_str("\\r"),
                _ if char::from(b) == quote => {
                    out.push('\\');
                    out.push(quote);
                }
                0x20..=0x7e => out.push(char::from(b)),
                _ => {
                    let _ = write!(out, "\\x{b:02x}");
                }
            }
        }
        out.push(quote);
        out
    }

    /// The bytes as the text a result shows, which is how ansible-core reads them.
    pub(crate) fn text(&self) -> Result<&str, String> {
        std::str::from_utf8(&self.0).map_err(|e| {
            format!(
                "bytes that are not UTF-8 cannot be a result: {} is not valid at byte {}",
                self.python_repr(),
                e.valid_up_to()
            )
        })
    }
}

impl Object for Bytes {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let at = key.as_i64()?;
        let at = if at < 0 {
            at.checked_add(i64::try_from(self.0.len()).ok()?)?
        } else {
            at
        };
        self.0
            .get(usize::try_from(at).ok()?)
            .map(|b| Value::from(*b))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Seq(self.0.len())
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.python_repr())
    }
}

/// The bytes a value holds, when it is a bytes value.
pub(crate) fn of(value: &Value) -> Option<&Bytes> {
    value.downcast_object_ref::<Bytes>()
}

/// Every method `bytes` has in CPython 3.13.
const METHODS: [&str; 42] = [
    "capitalize",
    "center",
    "count",
    "decode",
    "endswith",
    "expandtabs",
    "find",
    "fromhex",
    "hex",
    "index",
    "isalnum",
    "isalpha",
    "isascii",
    "isdigit",
    "islower",
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

/// A method of `bytes` called on `me`. A name `bytes` has and this engine does not run is `None`,
/// which the caller words as not available yet; a name `bytes` lacks is an error here.
pub(super) fn call(me: &Bytes, name: &str, args: &[Value]) -> Option<Result<Value, Error>> {
    let call = Call::of("bytes", args);
    match name {
        "decode" => Some(decode(me, &call)),
        "hex" => Some(hex(me, &call)),
        _ if METHODS.contains(&name) => None,
        _ => Some(Err(raise(format!(
            "object of type 'bytes' has no attribute '{name}'"
        )))),
    }
}

/// An argument that must be a text, as `encode` and `decode` word the failure.
fn text_arg<'v>(method: &str, what: &str, value: &'v Value) -> Result<&'v str, Error> {
    value.as_str().ok_or_else(|| {
        let found = if value.is_none() {
            "None"
        } else {
            type_of(value)
        };
        raise(format!(
            "{method}() argument '{what}' must be str, not {found}"
        ))
    })
}

/// `str.encode(encoding='utf-8', errors='strict')`.
pub(super) fn encode(text: &str, call: &Call<'_>) -> Result<Value, Error> {
    let slots = call.named("encode", &["encoding", "errors"])?;
    let encoding = slots[0].map_or(Ok("utf-8"), |v| text_arg("encode", "encoding", v))?;
    let errors = slots[1].map_or(Ok("strict"), |v| text_arg("encode", "errors", v))?;
    let codec = codec::lookup(encoding)?;
    check_encode(text.len(), codec, errors)?;
    Ok(Bytes::value(codec::encode(text, codec, errors)?))
}

/// `Ok` when encoding `len` bytes of text stays within the size limit. The longest result per
/// byte of text: a character is 4 bytes in UTF-32, and the escape of a character that 4 bytes of
/// text hold is at most 10 characters (`&#1114111;`).
pub(super) fn check_encode(len: usize, codec: codec::Codec, errors: &str) -> Result<(), Error> {
    let per_byte = match codec {
        codec::Codec::Utf8 => 1,
        codec::Codec::Utf16(..) => 2,
        codec::Codec::Utf32(..) => 4,
        _ if matches!(errors, "backslashreplace" | "xmlcharrefreplace") => 3,
        _ => 1,
    };
    check(len as u128 * per_byte + 4).map(drop)
}

/// `Ok` when decoding `len` bytes stays within the size limit: a byte that `backslashreplace`
/// escapes becomes four characters.
pub(super) fn check_decode(len: usize, errors: &str) -> Result<(), Error> {
    let per_byte = if errors == "backslashreplace" { 4 } else { 1 };
    check(len as u128 * per_byte).map(drop)
}

/// `Ok` when the hex text of `len` bytes, with its separators, stays within the size limit.
pub(super) fn check_hex(len: usize) -> Result<(), Error> {
    check(len as u128 * 3).map(drop)
}

/// `bytes.decode(encoding='utf-8', errors='strict')`.
fn decode(me: &Bytes, call: &Call<'_>) -> Result<Value, Error> {
    let slots = call.named("decode", &["encoding", "errors"])?;
    let encoding = slots[0].map_or(Ok("utf-8"), |v| text_arg("decode", "encoding", v))?;
    let errors = slots[1].map_or(Ok("strict"), |v| text_arg("decode", "errors", v))?;
    let codec = codec::lookup(encoding)?;
    check_decode(me.0.len(), errors)?;
    codec::decode(&me.0, codec, errors).map(Value::from)
}

/// `bytes.hex(sep, bytes_per_sep=1)`: two lower-case digits for each byte, with `sep` between
/// groups of `bytes_per_sep` bytes, counted from the right when the count is positive.
fn hex(me: &Bytes, call: &Call<'_>) -> Result<Value, Error> {
    let slots = call.named("hex", &["sep", "bytes_per_sep"])?;
    let sep = match slots[0] {
        None => None,
        Some(v) => {
            let sep = v.as_str().ok_or_else(|| {
                raise(if v.is_none() {
                    "sep must be str or bytes.".to_string()
                } else {
                    format!("sep must be str or bytes, not {}", type_of(v))
                })
            })?;
            if sep.chars().count() != 1 {
                return Err(raise("sep must be length 1."));
            }
            if !sep.is_ascii() {
                return Err(raise("sep must be ASCII."));
            }
            sep.chars().next()
        }
    };
    let group = slots[1].map_or(Ok(1), index)?;
    check_hex(me.0.len())?;
    let digits: Vec<String> = me.0.iter().map(|b| format!("{b:02x}")).collect();
    let (Some(sep), true) = (sep, group != 0 && !digits.is_empty()) else {
        return Ok(Value::from(digits.concat()));
    };
    let size = usize::try_from(group.unsigned_abs()).unwrap_or(usize::MAX);
    let mut out = String::new();
    let len = digits.len();
    for (at, pair) in digits.iter().enumerate() {
        // From the right a group ends at the last byte; from the left it begins at the first.
        let boundary = if group > 0 {
            (len - at).is_multiple_of(size) && at != 0
        } else {
            at.is_multiple_of(size) && at != 0
        };
        if boundary {
            out.push(sep);
        }
        out.push_str(pair);
    }
    Ok(Value::from(out))
}

/// Whether `value`, or something it holds, is a bytes value. Asked only after a template made
/// one.
pub(crate) fn holds(value: &Value) -> bool {
    if !MADE.load(Ordering::Relaxed) {
        return false;
    }
    match value.kind() {
        _ if of(value).is_some() => true,
        ValueKind::Seq | ValueKind::Iterable => value
            .try_iter()
            .is_ok_and(|mut items| items.any(|item| holds(&item))),
        ValueKind::Map => value
            .try_iter()
            .is_ok_and(|mut keys| keys.any(|k| value.get_item(&k).is_ok_and(|v| holds(&v)))),
        _ => false,
    }
}

/// How ansible-core opens the refusal to store a type or a function as a result.
const UNSTORABLE: &str = "Error rendering template: ";

/// The message of a result that cannot become JSON: the refusal to store a type is worded as
/// ansible-core words it, any other failure as a conversion.
pub(crate) fn result_error(error: &str) -> String {
    if error.starts_with(UNSTORABLE) {
        error.to_string()
    } else {
        format!("cannot convert template result: {error}")
    }
}

/// `bigint::to_json`, with every bytes value in `value` read as its text first. A type or a
/// function is no result: it fails as ansible-core's refusal to store one.
pub(crate) fn to_json(value: &Value) -> Result<serde_json::Value, String> {
    if let Some(kind) = super::super::globals::unstorable(value) {
        return Err(format!(
            "{UNSTORABLE}Type '{kind}' is unsupported for variable storage."
        ));
    }
    let json = if holds(value) {
        super::super::bigint::to_json(&settle(value)?)
    } else {
        super::super::bigint::to_json(value)
    };
    json.map_err(|e| e.to_string())
}

/// `value` with each bytes value, however deep, replaced by its text.
fn settle(value: &Value) -> Result<Value, String> {
    if let Some(bytes) = of(value) {
        return bytes.text().map(Value::from);
    }
    if !holds(value) {
        return Ok(value.clone());
    }
    match value.kind() {
        ValueKind::Seq | ValueKind::Iterable => {
            let items = value.try_iter().map_err(|e| e.to_string())?;
            items
                .map(|item| settle(&item))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::from)
        }
        _ => {
            let keys = value.try_iter().map_err(|e| e.to_string())?;
            let mut pairs = Vec::new();
            for key in keys {
                let item = value.get_item(&key).map_err(|e| e.to_string())?;
                pairs.push((key, settle(&item)?));
            }
            Ok(Value::from_pairs(pairs))
        }
    }
}
