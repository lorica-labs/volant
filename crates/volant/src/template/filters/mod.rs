// SPDX-License-Identifier: GPL-3.0-or-later
//! Ansible's filters, tests and lookups on top of MiniJinja's Jinja2 builtins. Each one keeps
//! Ansible's argument names and its edge cases, checked by tests/golden.

use std::path::PathBuf;

use minijinja::value::{Kwargs, Rest, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State, Value};

mod collections;
mod int;
mod jinja;
mod json;
mod ordering;
mod regex;
mod surrogates;
mod text;

use collections::{SetFilter, combine, dict2items, extract, flatten, items2dict, set_filter};
use int::to_int;
pub(crate) use json::python_json;
use json::{from_json, from_yaml, to_json, to_nice_json};
use regex::{regex_escape, regex_findall, regex_replace, regex_search, regex_test};
use text::{b64decode, b64encode, basename, comment, dirname, ipwrap, quote, split, to_uuid};

use super::pymethods::format::to_str;
use super::truthy;
use super::{add_filter, add_test, add_text_filter};

#[expect(clippy::too_many_lines)]
pub fn register(
    env: &mut Environment<'static>,
    base_dir: PathBuf,
    bridge: Option<&std::sync::Arc<crate::bridge::Bridge>>,
) {
    env.add_filter("volant_unsafe", text::volant_unsafe);
    // What a `!!binary` value loads as: its base64 text, read back as bytes.
    env.add_filter("volant_binary", |data: String| {
        volant_protocol::encoding::b64_decode(&data)
            .map(super::pymethods::bytes::Bytes::value)
            .map_err(|e| invalid(format!("Failed to convert base64 data: {e}")))
    });
    // What a `!vault` value loads as: its ciphertext, read when the value is rendered. A value
    // nothing renders is never read, so one that cannot be decrypted raises nothing until used.
    env.add_filter("volant_vault", |state: &State, ciphertext: String| {
        let plain = crate::vault::reveal(&ciphertext).map_err(|reason| {
            super::bridged::raise(crate::vault::undecryptable(&ciphertext, &reason))
        })?;
        super::lookups::taint(state);
        Ok::<_, Error>(plain)
    });
    add_filter(
        env,
        "vault",
        |data: String, secret: String, kwargs: Kwargs| {
            let id: Option<String> = kwargs.get("vault_id")?;
            let id = id.unwrap_or_else(|| "filter_default".into());
            // The salt and the wrapped object are read here, so they are refused rather than
            // dropped.
            if kwargs
                .get::<Option<Value>>("salt")?
                .is_some_and(|v| !v.is_none())
            {
                return Err(invalid("the salt argument of vault is not supported yet"));
            }
            if truthy_value(kwargs.get::<Option<Value>>("wrap_object")?) {
                return Err(invalid(
                    "the wrap_object argument of vault is not supported yet",
                ));
            }
            kwargs.assert_all_used()?;
            crate::vault::encrypt(data.as_bytes(), secret.as_bytes(), Some(&id))
                .map_err(|_| invalid("Unable to encrypt."))
        },
    );
    add_filter(
        env,
        "unvault",
        |vault: String, secret: String, kwargs: Kwargs| {
            let _: Option<String> = kwargs.get("vault_id")?;
            kwargs.assert_all_used()?;
            if !crate::vault::is_encrypted(vault.as_bytes()) {
                return Ok(vault);
            }
            let secrets = crate::vault::Secrets::new(vec![secret.into_bytes()]);
            crate::vault::decrypt(vault.as_bytes(), &secrets)
                .map_err(|_| invalid("Unable to decrypt."))
                .map(|plain| String::from_utf8_lossy(&plain).into_owned())
        },
    );
    // `NAME is vault_encrypted` is rewritten to carry the name (see `route_calls`): the
    // reference tells a vaulted variable by a tag this engine does not carry. The answer is the
    // current host's: the names are kept per host.
    add_test(env, "vault_encrypted", |state: &State, value: Value| {
        let host = state.lookup("inventory_hostname");
        value
            .as_str()
            .and_then(|s| s.strip_prefix(super::VAULT_NAME))
            .is_some_and(|name| {
                crate::vault::is_name(
                    host.as_ref().and_then(Value::as_str).unwrap_or_default(),
                    name,
                )
            })
    });
    add_test(env, "vaulted_file", |path: String| {
        std::fs::File::open(&path)
            .and_then(|mut f| {
                use std::io::Read as _;
                let mut head = Vec::new();
                f.by_ref().take(14).read_to_end(&mut head)?;
                Ok(crate::vault::is_encrypted(&head))
            })
            .map_err(|_| invalid(format!("Cannot test if the file '{path}' is a vault.")))
    });
    // minijinja's own filters and tests, registered again so that an undefined input stays
    // undefined through them as well (see `add_filter`), and so that they answer under
    // `ansible.builtin.` the way the reference's Jinja builtins do (`[1] | ansible.builtin.length`
    // is 1, measured on ansible-core 2.19.12). Those this engine replaces are registered below.
    {
        use minijinja::filters as f;
        add_filter(env, "safe", f::safe);
        add_filter(env, "escape", f::escape);
        add_filter(env, "e", f::escape);
        add_filter(env, "lower", f::lower);
        add_filter(env, "upper", f::upper);
        add_filter(env, "title", f::title);
        add_filter(env, "capitalize", f::capitalize);
        add_filter(env, "replace", jinja::replace);
        add_filter(env, "length", f::length);
        add_filter(env, "count", f::length);
        add_filter(env, "dictsort", f::dictsort);
        add_filter(env, "items", f::items);
        add_filter(env, "reverse", f::reverse);
        add_filter(env, "trim", f::trim);
        add_text_filter(env, "join", f::join);
        add_filter(env, "lines", f::lines);
        add_filter(env, "round", jinja::round);
        add_filter(env, "abs", f::abs);
        add_filter(env, "attr", f::attr);
        add_filter(env, "first", f::first);
        add_filter(env, "last", f::last);
        add_filter(env, "min", ordering::min);
        add_filter(env, "max", ordering::max);
        add_filter(env, "sort", ordering::sort);
        add_filter(env, "list", f::list);
        add_text_filter(env, "string", string);
        add_filter(env, "batch", f::batch);
        add_filter(env, "slice", f::slice);
        add_text_filter(env, "sum", jinja::sum);
        add_filter(env, "indent", super::pymethods::limits::indent);
        add_filter(env, "select", f::select);
        add_filter(env, "reject", f::reject);
        add_filter(env, "selectattr", f::selectattr);
        add_filter(env, "rejectattr", f::rejectattr);
        add_filter(env, "map", super::missing::map_naming);
        add_filter(env, "groupby", f::groupby);
        add_filter(env, "unique", ordering::unique);
        add_filter(env, "chain", f::chain);
        add_filter(env, "zip", f::zip);
        add_filter(env, "pprint", f::pprint);
        add_filter(env, "format", super::pymethods::format_filter);
    }
    {
        use minijinja::tests as t;
        add_test(env, "undefined", t::is_undefined);
        add_test(env, "defined", t::is_defined);
        add_test(env, "none", t::is_none);
        add_test(env, "safe", t::is_safe);
        add_test(env, "escaped", t::is_safe);
        add_test(env, "boolean", t::is_boolean);
        add_test(env, "odd", t::is_odd);
        add_test(env, "even", t::is_even);
        add_test(env, "divisibleby", t::is_divisibleby);
        add_test(env, "number", |v: Value| {
            t::is_number(&v) || super::bigint::is_big(&v)
        });
        for name in ["integer", "int"] {
            add_test(env, name, |v: Value| {
                t::is_integer(&v) || super::bigint::is_big(&v)
            });
        }
        add_test(env, "float", t::is_float);
        add_test(env, "string", t::is_string);
        add_test(env, "sequence", t::is_sequence);
        add_test(env, "iterable", t::is_iterable);
        add_test(env, "mapping", t::is_mapping);
        add_test(env, "startingwith", t::is_startingwith);
        add_test(env, "endingwith", t::is_endingwith);
        // Jinja2 tests `str(value)`, so `['A'] is upper` is true; minijinja fails every value
        // that is not a string.
        add_test(env, "lower", |v: Value| {
            t::is_lower(&Value::from(to_str(&v)))
        });
        add_test(env, "upper", |v: Value| {
            t::is_upper(&Value::from(to_str(&v)))
        });
        add_test(env, "sameas", t::is_sameas);
        for name in ["eq", "equalto", "=="] {
            add_test(env, name, t::is_eq);
        }
        for name in ["ne", "!="] {
            add_test(env, name, t::is_ne);
        }
        for name in ["lt", "lessthan", "<"] {
            add_test(env, name, t::is_lt);
        }
        for name in ["le", "<="] {
            add_test(env, name, t::is_le);
        }
        for name in ["gt", "greaterthan", ">"] {
            add_test(env, name, t::is_gt);
        }
        for name in ["ge", ">="] {
            add_test(env, name, t::is_ge);
        }
        add_test(env, "in", t::is_in);
        add_test(env, "true", t::is_true);
        add_test(env, "false", t::is_false);
        add_test(env, "filter", t::is_filter);
        add_test(env, "test", t::is_test);
    }

    add_filter(env, "default", default);
    add_filter(env, "d", default);
    add_filter(env, "bool", to_bool);
    add_filter(env, "int", to_int);
    add_filter(env, "float", to_float);
    add_filter(env, "mandatory", mandatory);
    add_filter(env, "ternary", ternary);
    add_filter(env, "combine", combine);
    add_filter(env, "dict2items", dict2items);
    add_filter(env, "items2dict", items2dict);
    add_filter(env, "to_json", to_json);
    add_filter(env, "to_nice_json", to_nice_json);
    add_filter(env, "from_json", |value: Value, kwargs: Kwargs| {
        // `profile` picks how ansible-core decodes the data's tags, which this engine does not
        // keep: measured, `from_json(profile="inventory_legacy")` parses as `from_json` does.
        let _profile = kwargs.get::<Option<Value>>("profile")?;
        kwargs.assert_all_used()?;
        from_json(value)
    });
    add_filter(env, "basename", basename);
    add_filter(env, "dirname", dirname);
    add_filter(env, "split", split);
    add_filter(env, "regex_replace", regex_replace);
    add_filter(env, "regex_search", regex_search);
    add_filter(env, "regex_findall", regex_findall);
    add_filter(env, "b64decode", b64decode);
    add_filter(env, "b64encode", b64encode);
    add_filter(env, "comment", comment);
    add_filter(env, "difference", |a: Value, b: Value| {
        set_filter(&a, &b, SetFilter::Difference)
    });
    add_filter(env, "intersect", |a: Value, b: Value| {
        set_filter(&a, &b, SetFilter::Intersect)
    });
    add_filter(env, "union", |a: Value, b: Value| {
        set_filter(&a, &b, SetFilter::Union)
    });
    add_filter(env, "flatten", flatten);
    add_filter(env, "from_yaml", from_yaml);
    add_filter(env, "to_uuid", to_uuid);
    // Measured on ansible-core 2.19.12: `nope | type_debug` is `UndefinedMarker`.
    add_filter(env, "type_debug", |v: Value| {
        if v.is_undefined() {
            "UndefinedMarker"
        } else if v.downcast_object_ref::<super::clock::DateTime>().is_some() {
            "datetime"
        } else if v.downcast_object_ref::<super::clock::TimeDelta>().is_some() {
            "timedelta"
        } else if v.is_tuple() {
            "tuple"
        } else if super::pymethods::bytes::of(&v).is_some() {
            "bytes"
        } else {
            super::type_name(&json(&v))
        }
    });
    add_text_filter(env, "quote", quote);
    add_filter(env, "regex_escape", regex_escape);
    add_filter(env, "extract", extract);
    // Not through `add_filter`: `ansible.utils.ipwrap` is a collection filter, and the reference
    // never exposes it as `ansible.builtin.ipwrap`.
    env.add_filter(
        "ansible.utils.ipwrap",
        super::undefined_through("ipwrap", ipwrap),
    );
    super::yaml_dump::register(env);

    super::test_plugins::register(env);
    add_test(env, "truthy", truthy_test);
    // `math.isnan`; a value that is no number is not a NaN, as the reference's test answers.
    add_test(env, "isnan", |v: Value| {
        v.kind() == ValueKind::Number && f64::try_from(v).is_ok_and(f64::is_nan)
    });
    add_test(
        env,
        "falsy",
        |v: Value, convert: Option<bool>, kwargs: Kwargs| {
            truthy_test(v, convert, kwargs).map(|t| !t)
        },
    );
    for name in ["match", "search", "regex"] {
        add_test(
            env,
            name,
            move |v: Value, rest: Rest<Value>, kwargs: Kwargs| regex_test(&v, &rest, &kwargs, name),
        );
    }
    // `item in seq`: a substring of a string, a key of a mapping, an element of anything else.
    add_test(env, "contains", |seq: Value, item: Value| {
        if let (Some(text), Some(needle)) = (seq.as_str(), item.as_str()) {
            return text.contains(needle);
        }
        if seq.kind() == ValueKind::Map {
            return seq.get_item(&item).is_ok_and(|v| !v.is_undefined());
        }
        seq.try_iter().is_ok_and(|mut it| it.any(|x| x == item))
    });

    super::lookups::register(env, base_dir, bridge);

    super::pymethods::register(env);
}

fn json(v: &Value) -> serde_json::Value {
    super::bigint::to_json(v).unwrap_or(serde_json::Value::Null)
}

/// `json`, for a filter whose result carries the value on. An undefined value inside it would
/// come out as `null`; measured on ansible-core 2.19.12, `{'k': nope} | dict2items`,
/// `[[nope]] | flatten` and `{'a': 1} | combine({'k': nope})` are undefined reads.
fn data(v: &Value) -> Result<serde_json::Value, Error> {
    if super::holds_undefined(v) {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    if super::pymethods::bytes::holds(v) {
        return Err(invalid("this filter does not take bytes yet"));
    }
    Ok(json(v))
}

fn from_json_value(v: serde_json::Value) -> Value {
    super::bigint::to_template(&v)
}

/// `truthy(value, convert_bool=False)`: with `convert_bool`, a text `boolean` reads (`'on'`,
/// `'off'`, `'no'`...) counts as that boolean, and anything else by its truth.
fn truthy_test(v: Value, convert: Option<bool>, kwargs: Kwargs) -> Result<bool, Error> {
    let convert = match convert {
        Some(c) => c,
        None => kwargs.get::<Option<bool>>("convert_bool")?.unwrap_or(false),
    };
    kwargs.assert_all_used()?;
    let v = json(&v);
    if convert && let serde_json::Value::String(text) = &v {
        match text.trim().to_lowercase().as_str() {
            "y" | "yes" | "on" | "1" | "true" | "t" => return Ok(true),
            "n" | "no" | "off" | "0" | "false" | "f" => return Ok(false),
            _ => {}
        }
    }
    Ok(truthy(&v))
}

fn truthy_value(value: Option<Value>) -> bool {
    value.is_some_and(|v| v.is_true())
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, msg.into())
}

/// `default(value, other='', boolean=False)`: `other` when `value` is undefined, or when
/// `boolean` is set and `value` is false-y. The arguments are read as they were passed, so that
/// `default(none)` gives `None` (measured on ansible-core 2.19.12) where `Option` would read
/// "not given". Either may also be passed by its name, `default_value` and `boolean`, as Jinja's
/// `do_default` takes them: measured, `false | default('passed', boolean=true)` is `passed`.
fn default(value: Value, args: Rest<Value>, kwargs: Kwargs) -> Result<Value, Error> {
    let named_other = kwargs.get::<Option<Value>>("default_value")?;
    let named_boolean = kwargs.get::<Option<Value>>("boolean")?;
    kwargs.assert_all_used()?;
    let (other, boolean) = match args.as_slice() {
        [] => (Value::from(""), false),
        [other] => (other.clone(), false),
        [other, boolean] => (other.clone(), boolean.is_true()),
        _ => {
            return Err(Error::new(
                ErrorKind::TooManyArguments,
                "default() takes at most 2 arguments",
            ));
        }
    };
    let other = named_other.unwrap_or(other);
    let boolean = named_boolean.map_or(boolean, |b| b.is_true());
    Ok(
        if value.is_undefined() || (boolean && !truthy(&json(&value))) {
            other
        } else {
            value
        },
    )
}

/// The `bool` filter's own set of spellings, which is **not** PyYAML's (see
/// [`crate::yaml::PYYAML_TRUE`]): it lower-cases the string and compares it against
/// `yes`, `on`, `1` and `true` only. Measured against ansible-core 2.19.12: `'yEs' | bool` is
/// true while `'y' | bool` and `'t' | bool` are false, and surrounding space is not stripped,
/// so a padded spelling is false too. Numbers are true only at exactly one (`1`, `1.00`), and
/// everything else, a non-empty list included, is false. `none | bool` is `false` too: measured
/// against ansible-core 2.19.12, `to_bool` coerces `None` to `False` (with a deprecation warning
/// that a future release will stop doing this), not the unchanged argument a stricter reading of
/// `to_bool`'s source would predict.
fn to_bool(value: Value) -> bool {
    match json(&value) {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Number(n) => n.as_f64() == Some(1.0),
        serde_json::Value::String(s) => {
            matches!(s.to_lowercase().as_str(), "yes" | "on" | "1" | "true")
        }
        _ => false,
    }
}

fn to_float(value: Value, default_value: Option<f64>) -> Value {
    let fallback = default_value.unwrap_or(0.0);
    Value::from(match json(&value) {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(fallback),
        serde_json::Value::Bool(b) => {
            if b {
                1.0
            } else {
                0.0
            }
        }
        serde_json::Value::String(s) => s.trim().parse::<f64>().unwrap_or(fallback),
        _ => fallback,
    })
}

fn mandatory(value: Value, msg: Option<String>) -> Result<Value, Error> {
    if value.is_undefined() {
        return Err(invalid(
            msg.unwrap_or_else(|| "Mandatory variable not defined.".to_string()),
        ));
    }
    Ok(value)
}

fn ternary(value: Value, true_val: Value, false_val: Value, none_val: Option<Value>) -> Value {
    if value.is_none()
        && let Some(none_val) = none_val
    {
        return none_val;
    }
    if truthy(&json(&value)) {
        true_val
    } else {
        false_val
    }
}

#[cfg(test)]
mod tests;

/// `string`, which prints a `range` or a `datetime` as Python does and leaves every other value
/// to MiniJinja.
fn string(state: &State, value: Value) -> Result<Value, Error> {
    // Python's `str` of a float: `nan`, `inf`, `1e+16`.
    if value.kind() == ValueKind::Number && !value.is_integer() {
        return Ok(Value::from(to_str(&value)));
    }
    if super::pymethods::format::holds_tuple(&value) {
        return Ok(Value::from(super::pymethods::format::repr(&value)));
    }
    if value.downcast_object_ref::<super::env::Range>().is_some()
        || value
            .downcast_object_ref::<super::clock::DateTime>()
            .is_some()
    {
        return Ok(Value::from(
            super::env::container_repr(&value).unwrap_or_default(),
        ));
    }
    minijinja::filters::string(state, &value)
}
