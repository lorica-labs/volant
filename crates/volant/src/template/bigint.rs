// SPDX-License-Identifier: GPL-3.0-or-later
//! Integers wider than 128 bits, and the two conversions between JSON and template values.
//!
//! A module can return any Python integer: `community.crypto` reports an RSA modulus of 4096
//! bits. `serde_json` keeps such a number as text (the `arbitrary_precision` feature), so a
//! result, a variable or a `to_json` prints every digit. A template value has no number that
//! wide, so one becomes a [`BigInt`] that prints as the same digits and goes back to JSON as a
//! number. Arithmetic on it fails; arithmetic up to 128 bits is native.
//!
//! `Value::from(Serde(..))` cannot read a `serde_json::Number` in this mode: the number
//! serializes as a one-field struct. [`to_template`] and [`to_json`] replace it for every
//! JSON value, and for every template value, that crosses the boundary.

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

use minijinja::value::{DynObject, Object, ObjectRepr, Value, ValueKind};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

/// An integer outside `i128` and `u128`: its sign and its decimal digits without leading zeros.
#[derive(Debug)]
struct BigInt {
    negative: bool,
    digits: String,
}

impl BigInt {
    /// The size of the number, ignoring its sign, against the digits of another.
    fn magnitude_cmp(&self, digits: &str) -> Ordering {
        self.digits
            .len()
            .cmp(&digits.len())
            .then_with(|| self.digits.as_str().cmp(digits))
    }

    fn order(&self, other: &Self) -> Ordering {
        let by_size = self.magnitude_cmp(&other.digits);
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => by_size,
            (true, true) => by_size.reverse(),
        }
    }

    fn sign(&self) -> Ordering {
        if self.negative {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    }

    fn number(&self) -> serde_json::Number {
        self.to_string()
            .parse()
            .expect("decimal digits are a JSON number")
    }
}

impl fmt::Display for BigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.negative {
            f.write_str("-")?;
        }
        f.write_str(&self.digits)
    }
}

impl Object for BigInt {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }

    fn custom_cmp(self: &Arc<Self>, other: &DynObject) -> Option<Ordering> {
        Some(self.order(other.downcast_ref::<Self>()?))
    }
}

/// The template value of an integer written in decimal, with an optional sign: native when it
/// fits 128 bits, a [`BigInt`] when it does not. `None` when `text` is not such an integer.
pub(crate) fn integer(text: &str) -> Option<Value> {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Ok(n) = text.parse::<i128>() {
        return Some(Value::from(n));
    }
    if let Ok(n) = text.parse::<u128>() {
        return Some(Value::from(n));
    }
    let digits = body.trim_start_matches('0').to_string();
    Some(Value::from_object(BigInt { negative, digits }))
}

/// `<` between a wide integer and another number, as Python orders them, exactly: `None` when
/// neither side is a wide integer or the other side is no number, `Some(None)` against `nan`.
pub(crate) fn order(a: &Value, b: &Value) -> Option<Option<Ordering>> {
    match (
        a.downcast_object_ref::<BigInt>(),
        b.downcast_object_ref::<BigInt>(),
    ) {
        (Some(x), Some(y)) => Some(Some(x.order(y))),
        (Some(x), None) => against(x, b),
        (None, Some(y)) => against(y, a).map(|order| order.map(Ordering::reverse)),
        (None, None) => None,
    }
}

/// A wide integer against a value that is no wide integer. Every native integer and every float
/// below 2^127 in size is smaller in size than it, so its sign decides; a float past that holds
/// no fraction, and `{:.0}` prints its digits exactly.
fn against(big: &BigInt, other: &Value) -> Option<Option<Ordering>> {
    match other.kind() {
        ValueKind::Bool => Some(Some(big.sign())),
        ValueKind::Number if other.is_integer() => Some(Some(big.sign())),
        ValueKind::Number => {
            let f = f64::try_from(other.clone()).ok()?;
            if f.is_nan() {
                return Some(None);
            }
            let same_sign = f.is_sign_negative() == big.negative;
            if !same_sign || f.abs() < 2f64.powi(127) {
                return Some(Some(big.sign()));
            }
            if f.is_infinite() {
                return Some(Some(big.sign().reverse()));
            }
            let size = big.magnitude_cmp(&format!("{:.0}", f.abs()));
            Some(Some(if big.negative { size.reverse() } else { size }))
        }
        _ => None,
    }
}

/// Whether `value` is an integer too wide for a native template number.
pub(crate) fn is_big(value: &Value) -> bool {
    value.downcast_object_ref::<BigInt>().is_some()
}

fn number(n: &serde_json::Number) -> Value {
    if let Some(i) = n.as_i64() {
        return Value::from(i);
    }
    if let Some(u) = n.as_u64() {
        return Value::from(u);
    }
    let text = n.as_str();
    if !text.contains(['.', 'e', 'E'])
        && let Some(value) = integer(text)
    {
        return value;
    }
    Value::from(text.parse::<f64>().unwrap_or(f64::NAN))
}

/// The template value of a JSON value, every integer exact.
pub(crate) fn to_template(value: &serde_json::Value) -> Value {
    use serde_json::Value as Json;
    match value {
        Json::Null => Value::from(()),
        Json::Bool(b) => Value::from(*b),
        Json::Number(n) => number(n),
        Json::String(s) => Value::from(s.as_str()),
        Json::Array(items) => items.iter().map(to_template).collect(),
        Json::Object(map) => Value::from_pairs(
            map.iter()
                .map(|(key, item)| (Value::from(key.as_str()), to_template(item))),
        ),
    }
}

/// What to add to an operator's refusal, when the operand it names is an object: the value of an
/// integer wider than 128 bits is one to minijinja, which names no limit.
pub(crate) fn refusal_note(err: &minijinja::Error) -> &'static str {
    let refused = err.kind() == minijinja::ErrorKind::InvalidOperation
        && err
            .detail()
            .is_some_and(|d| d.starts_with("tried to use ") && d.contains("types plain object"));
    if refused {
        " (an integer wider than 128 bits takes part in no arithmetic)"
    } else {
        ""
    }
}

/// Serializes a template value as `Value` does, except that a [`BigInt`] is a JSON number.
struct Wire<'a>(&'a Value);

impl Serialize for Wire<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Some(big) = self.0.downcast_object_ref::<BigInt>() {
            return big.number().serialize(serializer);
        }
        match self.0.kind() {
            ValueKind::Seq | ValueKind::Iterable => {
                let mut seq = serializer.serialize_seq(None)?;
                if let Ok(items) = self.0.try_iter() {
                    for item in items {
                        seq.serialize_element(&Wire(&item))?;
                    }
                }
                seq.end()
            }
            ValueKind::Map => {
                let mut map = serializer.serialize_map(None)?;
                if let Ok(keys) = self.0.try_iter() {
                    for key in keys {
                        let item = self.0.get_item(&key).unwrap_or(Value::UNDEFINED);
                        map.serialize_entry(&key, &Wire(&item))?;
                    }
                }
                map.end()
            }
            _ => self.0.serialize(serializer),
        }
    }
}

/// The JSON value of a template value, every integer exact.
pub(crate) fn to_json(value: &Value) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(Wire(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("0", "0")]
    #[case(
        "-170141183460469231731687303715884105728",
        "-170141183460469231731687303715884105728"
    )]
    #[case(
        "340282366920938463463374607431768211455",
        "340282366920938463463374607431768211455"
    )]
    #[case(
        "340282366920938463463374607431768211456",
        "340282366920938463463374607431768211456"
    )]
    #[case(
        "-170141183460469231731687303715884105729",
        "-170141183460469231731687303715884105729"
    )]
    #[case("+00000000000000000000000000000000000000000000000000000001", "1")]
    #[case(
        "-00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001",
        "-1"
    )]
    fn integers_print_every_digit(#[case] text: &str, #[case] printed: &str) {
        assert_eq!(integer(text).expect("an integer").to_string(), printed);
    }

    #[rstest]
    #[case("")]
    #[case("-")]
    #[case("1.5")]
    #[case("1e5")]
    #[case("12a")]
    fn text_that_is_not_an_integer_is_refused(#[case] text: &str) {
        assert!(integer(text).is_none());
    }

    #[rstest]
    #[case("340282366920938463463374607431768211456", true)]
    #[case("340282366920938463463374607431768211455", false)]
    #[case("-170141183460469231731687303715884105729", true)]
    #[case("12345", false)]
    fn only_what_no_native_number_holds_is_big(#[case] text: &str, #[case] big: bool) {
        assert_eq!(is_big(&integer(text).unwrap()), big);
    }

    #[rstest]
    #[case(
        "340282366920938463463374607431768211456",
        "340282366920938463463374607431768211457",
        Ordering::Less
    )]
    #[case(
        "-340282366920938463463374607431768211457",
        "-340282366920938463463374607431768211456",
        Ordering::Less
    )]
    #[case(
        "-340282366920938463463374607431768211457",
        "340282366920938463463374607431768211457",
        Ordering::Less
    )]
    #[case(
        "340282366920938463463374607431768211457",
        "-340282366920938463463374607431768211457",
        Ordering::Greater
    )]
    #[case(
        "1000000000000000000000000000000000000000",
        "999999999999999999999999999999999999999999",
        Ordering::Less
    )]
    #[case(
        "340282366920938463463374607431768211456",
        "340282366920938463463374607431768211456",
        Ordering::Equal
    )]
    fn big_integers_compare_as_numbers(#[case] a: &str, #[case] b: &str, #[case] order: Ordering) {
        let (a, b) = (integer(a).unwrap(), integer(b).unwrap());
        assert_eq!(a.partial_cmp(&b), Some(order));
        assert_eq!(a == b, order == Ordering::Equal);
    }

    #[rstest]
    #[case(r#"{"n":1267650600228229401496703205377,"l":[-1,18446744073709551616],"f":1.5}"#)]
    #[case(&format!(r#"{{"n":{}}}"#, "9".repeat(1234)))]
    #[case(&format!("[-{}, 0, 7]", "1".repeat(80)))]
    #[case("[9223372036854775808, 18446744073709551615, -9223372036854775809]")]
    fn json_survives_a_round_trip_through_a_template_value(#[case] text: &str) {
        let json: serde_json::Value = serde_json::from_str(text).unwrap();
        let back = to_json(&to_template(&json)).unwrap();
        assert_eq!(back.to_string(), json.to_string());
    }
}
