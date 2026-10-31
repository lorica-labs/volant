// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a variable whose value is a template was written, so that a failure inside that template
//! is placed where the reference places it: `'nope' is undefined` at `ind: "{{ nope }}"`, under
//! the value that read `ind`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::playbook::Origin;

/// Each variable name and template text, and where it was written. A name that holds the same
/// text in two files keeps the first.
static WRITTEN_AT: LazyLock<Mutex<HashMap<(String, String), Origin>>> =
    LazyLock::new(Mutex::default);

pub(crate) fn note(key: &str, text: &str, origin: Origin) {
    WRITTEN_AT
        .lock()
        .expect("written lock")
        .entry((key.to_string(), text.to_string()))
        .or_insert(origin);
}

pub(crate) fn written_at(key: &str, text: &str) -> Option<Origin> {
    WRITTEN_AT
        .lock()
        .expect("written lock")
        .get(&(key.to_string(), text.to_string()))
        .cloned()
}

thread_local! {
    /// The last failure the naming of an undefined read took from inside a variable's template,
    /// and where that template was written.
    static INNER: RefCell<Option<(String, Option<Origin>)>> = const { RefCell::new(None) };
}

pub(super) fn set_inner(inner: Option<(String, Option<Origin>)>) {
    INNER.with(|i| *i.borrow_mut() = inner);
}

/// Where the template that failed with `message` inside a variable was written, when the last
/// undefined read named on this thread was that one: `Some(None)` for such a failure with no
/// known place, `None` for a failure of the template itself.
pub(crate) fn inner_origin(message: &str) -> Option<Option<Origin>> {
    INNER.with(|i| {
        i.borrow()
            .as_ref()
            .filter(|(said, _)| said == message)
            .map(|(_, origin)| origin.clone())
    })
}
