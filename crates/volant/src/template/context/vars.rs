// SPDX-License-Identifier: GPL-3.0-or-later
//! `vars`: every variable of the render, read by name as the template would read it.

use std::sync::Arc;

use minijinja::value::{Enumerator, Object, ObjectExt};
use minijinja::{Error, ErrorKind, State, Value};

use super::Context;

/// The magic `vars` of ansible-core 2.19.12: `vars['a']`, `vars[key]`, `'a' in vars`,
/// `vars | length` and `vars.get('a')` each read the variable as `{{ a }}` would, a template in it
/// rendered. `omit` is no variable, so `'omit' in vars` is false.
#[derive(Debug)]
pub(super) struct Vars(pub(super) Arc<Context>);

/// Names the render binds for its own use, which a template never reads through `vars`, and
/// `omit`, which the reference holds apart from the variables.
fn internal(key: &str) -> bool {
    key == "omit" || key.contains("::") || key.starts_with("__volant")
}

impl Object for Vars {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        if internal(key.as_str()?) {
            return None;
        }
        self.0.get_value(key)
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        match self.0.enumerate() {
            Enumerator::Values(keys) => Enumerator::Values(
                keys.into_iter()
                    .filter(|k| !k.as_str().is_some_and(internal))
                    .collect(),
            ),
            other => other,
        }
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let keys = || {
            self.try_iter()
                .into_iter()
                .flatten()
                .collect::<Vec<Value>>()
        };
        match (method, args) {
            ("get", [key]) => Ok(self.get_value(key).unwrap_or(Value::from(()))),
            ("get", [key, default]) => Ok(self.get_value(key).unwrap_or_else(|| default.clone())),
            ("keys", []) => Ok(Value::from(keys())),
            ("values", []) => Ok(Value::from(
                keys()
                    .iter()
                    .map(|k| self.get_value(k).unwrap_or_default())
                    .collect::<Vec<_>>(),
            )),
            ("items", []) => Ok(Value::from(
                keys()
                    .into_iter()
                    .map(|k| {
                        let v = self.get_value(&k).unwrap_or_default();
                        Value::from(vec![k, v])
                    })
                    .collect::<Vec<_>>(),
            )),
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

/// `dict(vars)`: a copy of every variable, each rendered, as the reference's copy reads them all.
/// A variable that renders to nothing fails the copy, as it fails the reference's. `None` for any
/// other value.
pub(in crate::template) fn copied(source: &Value) -> Option<Result<Value, Error>> {
    let vars = source.downcast_object::<Vars>()?;
    let mut pairs = Vec::new();
    for key in vars.try_iter().into_iter().flatten() {
        match vars.get_value(&key) {
            Some(value) if !value.is_undefined() => pairs.push((key, value)),
            _ => return Some(Err(Error::from(ErrorKind::UndefinedError))),
        }
    }
    Some(Ok(pairs.into_iter().collect()))
}
