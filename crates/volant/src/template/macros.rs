// SPDX-License-Identifier: GPL-3.0-or-later
//! What a macro call answers. ansible-core 2.19.12 joins the nodes a macro's body writes as it
//! joins a template's: a body that writes one `{{ }}` and nothing else gives that expression's
//! value, so a macro that writes a mapping answers a mapping. minijinja's macro answers the text
//! it wrote. Each call of a macro the template defines is routed here (`vm::route_calls`), and
//! the value comes back when the text is what that one expression printed.

use std::cell::RefCell;

use minijinja::value::{Rest, ValueOrKwargs};
use minijinja::{Error, State, Value};

thread_local! {
    /// What each macro call under way on this thread printed through `{{ }}`, innermost last.
    static CALLS: RefCell<Vec<Vec<Value>>> = const { RefCell::new(Vec::new()) };
}

/// Notes one `{{ }}` the VM prints, for the innermost macro call under way. A `none` is no node.
pub(super) fn note(value: &Value) {
    if value.is_none() {
        return;
    }
    CALLS.with_borrow_mut(|calls| {
        if let Some(call) = calls.last_mut() {
            call.push(value.clone());
        }
    });
}

/// The function a call of the macro `name` is routed through.
pub(super) fn call(name: String) -> Value {
    Value::from_function(
        move |state: &mut State, args: Rest<ValueOrKwargs>| -> Result<Value, Error> {
            let target = state.lookup(&name).unwrap_or_default();
            CALLS.with_borrow_mut(|calls| calls.push(Vec::new()));
            let answer = target.call(state, &args.into_values());
            let printed = CALLS.with_borrow_mut(Vec::pop).unwrap_or_default();
            let answer = answer?;
            Ok(match printed.as_slice() {
                [value] if answer.as_str() == Some(shown(value).as_str()) => value.clone(),
                _ => answer,
            })
        },
    )
}

/// The text `{{ value }}` prints.
fn shown(value: &Value) -> String {
    super::env::container_repr(value).unwrap_or_else(|| value.to_string())
}
