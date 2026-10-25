// SPDX-License-Identifier: GPL-3.0-or-later
//! The unknown value of `volant shadow replay`: what a lookup or a function that gives another
//! answer on every run (`pipe`, `env`, `password`, `now`, `random`, a file outside the playbook)
//! yields during a replay, and what a recorded `no_log` result holds, since its values were
//! never recorded.
//!
//! An expression that reads an unknown value is unknown, wholly: reading its type, an attribute
//! or an item of it, testing it as a condition, looping over it or printing it marks the render
//! under way, and that render's result is unknown. Between renders, in a variable or a module
//! argument, the value is a marker string naming the reason; reading a variable that holds one
//! gives the unknown value back. Outside a replay nothing here is ever reached.

use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use minijinja::value::{Enumerator, Object, ObjectRepr, Value};
use minijinja::{Error, State};

static REPLAY: AtomicBool = AtomicBool::new(false);

/// The key a replay puts on a recorded `no_log` result: a key read from it that it does not have
/// is unknown, since the recording kept nothing but its status.
pub(crate) const CENSORED: &str = "_volant_censored";

/// What opens and closes the marker text: two characters no playbook writes.
const OPEN: &str = "\u{E000}unknown:";
const CLOSE: char = '\u{E001}';

thread_local! {
    /// The reason of the first unknown value the render under way read.
    static TOUCHED: RefCell<Option<Arc<str>>> = const { RefCell::new(None) };
    #[cfg(test)]
    static TEST_REPLAY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Turns the unknown value on for the rest of the process: `volant shadow replay` only.
pub(crate) fn enable() {
    REPLAY.store(true, Ordering::Relaxed);
}

/// Whether this process replays.
pub(crate) fn active() -> bool {
    #[cfg(test)]
    if TEST_REPLAY.with(std::cell::Cell::get) {
        return true;
    }
    REPLAY.load(Ordering::Relaxed)
}

/// Turns the unknown value on for this test's thread alone.
#[cfg(test)]
pub(crate) fn enable_here() {
    TEST_REPLAY.with(|on| on.set(true));
}

/// The marker text an unknown value is kept as between renders.
pub(crate) fn marker(reason: &str) -> String {
    format!("{OPEN}{reason}{CLOSE}")
}

/// The reason of the first marker in `text`.
pub(crate) fn reason_in(text: &str) -> Option<&str> {
    let start = text.find(OPEN)? + OPEN.len();
    let end = text[start..].find(CLOSE)? + start;
    Some(&text[start..end])
}

/// The reason of the first marker anywhere in a JSON value, keys included.
pub(crate) fn reason_in_json(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => reason_in(s).map(str::to_owned),
        serde_json::Value::Array(items) => items.iter().find_map(reason_in_json),
        serde_json::Value::Object(map) => map.iter().find_map(|(k, v)| {
            reason_in(k)
                .map(str::to_owned)
                .or_else(|| reason_in_json(v))
        }),
        _ => None,
    }
}

/// The unknown value, `reason` saying where it came from (`lookup 'pipe'`).
pub(crate) fn value(reason: &str) -> Value {
    Value::from_object(Unknown(Arc::from(reason)))
}

/// A value read from a variable: the unknown value when it is a marker, or holds one.
pub(crate) fn from_marker(text: &str) -> Option<Value> {
    if !active() {
        return None;
    }
    reason_in(text).map(value)
}

/// What reading `name` from `parent` gave, `read`: the unknown value when `parent` is a
/// recorded `no_log` result that does not have it.
pub(crate) fn censored_read(parent: &Value, read: Value) -> Value {
    if read.is_undefined() && active() && parent.get_attr(CENSORED).is_ok_and(|v| v.is_true()) {
        return value("no_log");
    }
    read
}

/// Starts a render: what an earlier render read is put aside and given back by [`end`].
pub(crate) fn begin() -> Option<Arc<str>> {
    TOUCHED.with(|t| t.borrow_mut().take())
}

/// Ends a render begun with `before`: the reason of the unknown value it read, when it read one.
/// A render nested in another one leaves its reason to the outer render too.
pub(crate) fn end(before: Option<Arc<str>>) -> Option<Arc<str>> {
    TOUCHED.with(|t| {
        let mut touched = t.borrow_mut();
        let read = touched.take();
        *touched = before.or_else(|| read.clone());
        read
    })
}

fn touch(reason: &Arc<str>) {
    TOUCHED.with(|t| {
        t.borrow_mut().get_or_insert_with(|| Arc::clone(reason));
    });
}

#[derive(Debug)]
struct Unknown(Arc<str>);

impl Object for Unknown {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        touch(&self.0);
        ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, _key: &Value) -> Option<Value> {
        touch(&self.0);
        Some(Value::from_object(Unknown(Arc::clone(&self.0))))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        touch(&self.0);
        Enumerator::Empty
    }

    fn is_true(self: &Arc<Self>) -> bool {
        touch(&self.0);
        false
    }

    fn call(self: &Arc<Self>, _state: &mut State<'_, '_>, _args: &[Value]) -> Result<Value, Error> {
        touch(&self.0);
        Ok(Value::from_object(Unknown(Arc::clone(&self.0))))
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        _method: &str,
        _args: &[Value],
    ) -> Result<Value, Error> {
        touch(&self.0);
        Ok(Value::from_object(Unknown(Arc::clone(&self.0))))
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        touch(&self.0);
        f.write_str(&marker(&self.0))
    }
}

#[cfg(test)]
mod tests;
