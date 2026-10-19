// SPDX-License-Identifier: GPL-3.0-or-later
//! `lipsum(n=5, html=True, min=20, max=100)`, Jinja's filler text.
//!
//! The words are Jinja's, and so is the shape: `n` paragraphs of `min` up to but not counting
//! `max` words, with a comma now and then, a full stop that ends each sentence, a capital that
//! opens it, and no word twice in a row. The text is random, as it is in the reference.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use minijinja::value::{Rest, ValueKind, ValueOrKwargs};
use minijinja::{Error, ErrorKind, Value};

use super::globals::{Callee, bind, python_type};

/// Jinja's list of words, one space or line end apart.
const WORDS: &str = include_str!("lipsum_words.txt");

/// The most words and paragraphs one call makes. Ten million words are about 70 MB of text, far
/// above what a playbook needs; the reference has no bound and runs until it is out of memory.
const MAX_WORDS: i64 = 10_000_000;

fn raise(message: String) -> Error {
    Error::new(ErrorKind::InvalidOperation, message)
}

/// A SplitMix64 generator, seeded from the system as every `HashMap` of the process is.
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        Self(RandomState::new().build_hasher().finish())
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number from `low` up to but not counting `high`; `high` is above `low`.
    fn between(&mut self, low: i64, high: i64) -> i64 {
        let span = u64::try_from(i128::from(high) - i128::from(low)).unwrap_or(1);
        low + i64::try_from(self.next() % span).unwrap_or(0)
    }
}

/// Python's `randrange(low, high)` with `high` left out when the argument was `none`.
fn randrange(rng: &mut Rng, low: i64, high: Option<i64>) -> Result<i64, Error> {
    match high {
        None if low > 0 => Ok(rng.between(0, low)),
        None => Err(raise("empty range for randrange()".into())),
        Some(high) if high > low => Ok(rng.between(low, high)),
        Some(high) => Err(raise(format!("empty range in randrange({low}, {high})"))),
    }
}

/// An argument Python reads as an `int`: a whole number or a boolean.
fn integer(value: &Value) -> Result<i64, Error> {
    let refused = || {
        raise(format!(
            "'{}' object cannot be interpreted as an integer",
            python_type(value)
        ))
    };
    match value.kind() {
        ValueKind::Bool => Ok(i64::from(value.is_true())),
        ValueKind::Number if python_type(value) == "int" => {
            i64::try_from(value.clone()).map_err(|_| refused())
        }
        _ => Err(refused()),
    }
}

/// `lipsum`. Measured on ansible-core 2.19.12: `html` is read as a truth value, `max` may be
/// `none` (Python then draws from `0` up to `min`), and a bound is read only when a paragraph
/// is made, so `lipsum(0, true, 'a')` is empty.
pub(super) fn lipsum(args: Rest<ValueOrKwargs>) -> Result<Value, Error> {
    let args = args.into_values();
    let [count, html, min, max] = bind(
        Callee::Python,
        "generate_lorem_ipsum",
        &["n", "html", "min", "max"],
        &args,
    )?;
    let count = count.map_or(Ok(5), |count| integer(&count))?;
    let html = html.is_none_or(|html| html.is_true());
    let words: Vec<&str> = WORDS.split_whitespace().collect();
    let mut rng = Rng::new();
    let mut paragraphs = Vec::new();
    let mut made = 0_i64;
    for _ in 0..count {
        let low = min.as_ref().map_or(Ok(20), integer)?;
        let high = match &max {
            None => Some(100),
            Some(max) if max.is_none() => None,
            Some(max) => Some(integer(max)?),
        };
        let length = randrange(&mut rng, low, high)?;
        made = made.saturating_add(length.max(1));
        if made > MAX_WORDS {
            return Err(raise(format!(
                "lipsum() makes at most {MAX_WORDS} words in one call"
            )));
        }
        paragraphs.push(paragraph(&mut rng, &words, length));
    }
    Ok(Value::from(if html {
        paragraphs
            .iter()
            .map(|text| format!("<p>{text}</p>"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        paragraphs.join("\n\n")
    }))
}

/// One paragraph of `length` words.
fn paragraph(rng: &mut Rng, words: &[&str], length: i64) -> String {
    let mut text = Vec::new();
    let mut capitalize = true;
    let mut last_comma = 0;
    let mut last_stop = 0;
    let mut last = "";
    for at in 0..length {
        let mut word = loop {
            let next = words[usize::try_from(rng.next() % words.len() as u64).unwrap_or(0)];
            if next != last {
                last = next;
                break next.to_string();
            }
        };
        if capitalize {
            word[..1].make_ascii_uppercase();
            capitalize = false;
        }
        if at - rng.between(3, 8) > last_comma {
            last_comma = at;
            last_stop += 2;
            word.push(',');
        }
        if at - rng.between(10, 20) > last_stop {
            last_comma = at;
            last_stop = at;
            word.push('.');
            capitalize = true;
        }
        text.push(word);
    }
    let mut text = text.join(" ");
    if text.ends_with(',') {
        text.pop();
        text.push('.');
    } else if !text.ends_with('.') {
        text.push('.');
    }
    text
}
