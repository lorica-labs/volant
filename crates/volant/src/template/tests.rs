// SPDX-License-Identifier: GPL-3.0-or-later
//! The tests ansible-core ships in `ansible/plugins/test/core.py` that read a task result, and
//! `version`, which compares the way that file's default `LooseVersion` does.

use std::cmp::Ordering;

use minijinja::value::Kwargs;
use minijinja::{Environment, Error, ErrorKind, Value};
use serde_json::Value as Json;

use super::add_test;
use super::truthy;

/// Each name beside the function `core.py` maps it to, and the name that function's own
/// message uses: `succeeded` calls `failed`, so its refusal of a non-mapping says `'failed'`.
const RESULT_TESTS: [(&str, Which); 9] = [
    ("changed", Which::Changed),
    ("change", Which::Changed),
    ("failed", Which::Failed),
    ("failure", Which::Failed),
    ("succeeded", Which::Succeeded),
    ("success", Which::Succeeded),
    ("successful", Which::Succeeded),
    ("skipped", Which::Skipped),
    ("skip", Which::Skipped),
];

#[derive(Clone, Copy)]
enum Which {
    Changed,
    Failed,
    Succeeded,
    Skipped,
}

pub fn register(env: &mut Environment<'static>) {
    for (name, which) in RESULT_TESTS {
        add_test(env, name, move |value: Value| {
            result_test(name, which, &value)
        });
    }
    for name in ["version", "version_compare"] {
        add_test(
            env,
            name,
            move |value: Value, version: Value, operator: Option<String>, kwargs: Kwargs| {
                version_test(name, &value, &version, operator, &kwargs)
            },
        );
    }
}

/// The reference's wording for a test that raised: `The test plugin 'ansible.builtin.changed'
/// failed: The 'changed' test expects a dictionary`, measured on ansible-core 2.19.12.
fn failed_test(name: &str, msg: &str) -> Error {
    Error::new(
        ErrorKind::InvalidOperation,
        format!("The test plugin 'ansible.builtin.{name}' failed: {msg}"),
    )
}

fn result_test(name: &str, which: Which, value: &Value) -> Result<bool, Error> {
    // minijinja hands a test the undefined value rather than raising, so a missing name would
    // otherwise be judged as a non-mapping here instead of failing as an undefined read.
    if value.is_undefined() {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    let own = match which {
        Which::Changed => "changed",
        Which::Failed | Which::Succeeded => "failed",
        Which::Skipped => "skipped",
    };
    let Json::Object(result) = serde_json::to_value(value).unwrap_or(Json::Null) else {
        return Err(failed_test(
            name,
            &format!("The '{own}' test expects a dictionary"),
        ));
    };
    let key = |k: &str| result.get(k).is_some_and(truthy);
    Ok(match which {
        // `changed` falls back to a loop's items when the result has no key of its own, and
        // only when the first item is a mapping.
        Which::Changed => match result.get("changed") {
            Some(v) => truthy(v),
            None => match result.get("results") {
                Some(Json::Array(items)) if items.first().is_some_and(Json::is_object) => items
                    .iter()
                    .any(|item| item.get("changed").is_some_and(truthy)),
                _ => false,
            },
        },
        Which::Failed => key("failed"),
        Which::Succeeded => !key("failed"),
        Which::Skipped => key("skipped"),
    })
}

/// One component of a `LooseVersion`: a run of digits, compared as a number, or anything else,
/// compared as text.
#[derive(Debug, PartialEq, Eq)]
enum Part {
    /// The digits without their leading zeros, so that length then text is numeric order at any
    /// size, as Python's integers are.
    Int(String),
    Str(String),
}

/// `LooseVersion.parse`: split on `\d+ | [a-z]+ | \.`, keep the separators too, and drop the
/// empty pieces and the dots.
fn loose_version(text: &str) -> Vec<Part> {
    let re = regex::Regex::new(r"[0-9]+|[a-z]+|\.").expect("static pattern");
    let mut parts = Vec::new();
    let mut push = |piece: &str| {
        if piece.is_empty() || piece == "." {
            return;
        }
        parts.push(if piece.bytes().all(|b| b.is_ascii_digit()) {
            Part::Int(piece.trim_start_matches('0').to_string())
        } else {
            Part::Str(piece.to_string())
        });
    };
    let mut end = 0;
    for m in re.find_iter(text) {
        push(&text[end..m.start()]);
        push(m.as_str());
        end = m.end();
    }
    push(&text[end..]);
    parts
}

/// Python's list comparison: the first pair that differs decides, and a number against a string
/// is a `TypeError`. `LooseVersion._cmp` asks `==` first and then `<`, so the operator its
/// message names is always `<`, whichever comparison the playbook wrote.
fn compare(a: &[Part], b: &[Part]) -> Result<Ordering, String> {
    for (x, y) in a.iter().zip(b) {
        let order = match (x, y) {
            (Part::Int(x), Part::Int(y)) => (x.len(), x).cmp(&(y.len(), y)),
            (Part::Str(x), Part::Str(y)) => x.cmp(y),
            (x, y) => {
                let kind = |p: &Part| match p {
                    Part::Int(_) => "int",
                    Part::Str(_) => "str",
                };
                return Err(format!(
                    "'<' not supported between instances of '{}' and '{}'",
                    kind(x),
                    kind(y)
                ));
            }
        };
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(a.len().cmp(&b.len()))
}

const OPERATORS: [&str; 14] = [
    "==", "=", "eq", "<", "lt", "<=", "le", ">", "gt", ">=", "ge", "!=", "<>", "ne",
];

/// `version_compare(value, version, operator='eq', strict=None, version_type=None)` with the
/// default `LooseVersion`. `strict` and `version_type` pick another class, which no measured
/// role asks for, so they are refused by name rather than ignored.
fn version_test(
    name: &str,
    value: &Value,
    version: &Value,
    operator: Option<String>,
    kwargs: &Kwargs,
) -> Result<bool, Error> {
    if value.is_undefined() || version.is_undefined() {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    let operator = match operator {
        Some(op) => op,
        None => kwargs
            .get::<Option<String>>("operator")?
            .unwrap_or_else(|| "eq".into()),
    };
    for option in ["strict", "version_type"] {
        if kwargs
            .get::<Option<Value>>(option)?
            .is_some_and(|v| !v.is_none())
        {
            return Err(failed_test(
                name,
                &format!("{option}= is not supported yet"),
            ));
        }
    }
    kwargs.assert_all_used()?;
    let json = |v: &Value| serde_json::to_value(v).unwrap_or(Json::Null);
    if !truthy(&json(value)) {
        return Err(failed_test(name, "Input version value cannot be empty"));
    }
    if !truthy(&json(version)) {
        return Err(failed_test(
            name,
            "Version parameter to compare against cannot be empty",
        ));
    }
    if !OPERATORS.contains(&operator.as_str()) {
        let all: Vec<String> = OPERATORS.iter().map(|op| format!("'{op}'")).collect();
        return Err(failed_test(
            name,
            &format!(
                "Invalid operator type ({operator}). Must be one of {}",
                all.join(", ")
            ),
        ));
    }
    let text = |v: &Value| v.as_str().map_or_else(|| v.to_string(), str::to_string);
    let order = compare(&loose_version(&text(value)), &loose_version(&text(version)))
        .map_err(|e| failed_test(name, &format!("Version comparison failed: {e}")))?;
    Ok(match operator.as_str() {
        "==" | "=" | "eq" => order == Ordering::Equal,
        "<" | "lt" => order == Ordering::Less,
        "<=" | "le" => order != Ordering::Greater,
        ">" | "gt" => order == Ordering::Greater,
        ">=" | "ge" => order != Ordering::Less,
        _ => order != Ordering::Equal,
    })
}

#[cfg(test)]
mod unit {
    use serde_json::{Map, json};

    use super::super::Templar;

    fn templar() -> Templar {
        Templar::new(std::env::temp_dir())
    }

    /// Measured on ansible-core 2.19.12, in one expression each, and printed word for word: a
    /// boolean rendered into text is `True`/`False` in minijinja as it is in Jinja2.
    ///
    /// What would make this red: a test that reads a key the reference does not (`changed`
    /// taken from anything but the result's own key), or one that accepts a string as a result.
    #[test]
    fn the_result_tests_read_a_result_as_the_reference_does() {
        let t = templar();
        assert_eq!(
            t.render(
                "{{ {'a': 1} is changed }} {{ {'changed': true} is changed }} {{ {} is failed }} {{ {'failed': 1} is failed }} {{ {'skipped': true} is skipped }} {{ {'failed': false} is succeeded }} {{ {} is success }}",
                &Map::new()
            )
            .unwrap(),
            json!("False True False True True True True")
        );
        let err = t.render("{{ 'x' is changed }}", &Map::new()).unwrap_err();
        assert!(
            err.0.contains("The 'changed' test expects a dictionary"),
            "{err}"
        );
    }

    /// The aliases `core.py` maps to the same four functions, and the loop shape `changed`
    /// reads when a result has no `changed` key of its own but a list of `results`.
    ///
    /// What would make this red: an alias left out, or `changed` ignoring `results`.
    #[test]
    fn the_aliases_and_a_loop_result_answer_like_the_names() {
        let t = templar();
        assert_eq!(
            t.render(
                "{{ {'changed': 1} is change }} {{ {'failed': 1} is failure }} {{ {} is successful }} {{ {'skipped': 'yes'} is skip }} {{ {'results': [{}, {'changed': true}]} is changed }}",
                &Map::new()
            )
            .unwrap(),
            json!("True True True True True")
        );
    }

    /// A result test on a name that has no value fails as any other read of it does, not with
    /// the dictionary message: minijinja hands a test an undefined value rather than raising.
    ///
    /// What would make this red: the test reading the undefined value as an empty mapping or a
    /// non-mapping, which would say `false` or `expects a dictionary`.
    #[test]
    fn a_result_test_on_an_undefined_name_is_an_undefined_error() {
        let err = templar()
            .render("{{ nothing is changed }}", &Map::new())
            .unwrap_err();
        assert!(err.is_undefined(), "{err}");
    }

    /// LooseVersion, as the reference's `version` test defaults to. Measured on ansible-core
    /// 2.19.12: `True False False`, and the two failures below word for word.
    ///
    /// What would make this red: comparing the two versions as strings (`'1.10' < '1.9'`), or
    /// a component list that does not treat the shorter as the lesser.
    #[test]
    fn version_compares_the_reference_s_way() {
        let t = templar();
        assert_eq!(
            t.render(
                "{{ ('1.10' is version('1.9', '>')) }} {{ ('1.10' is version_compare('1.9', 'lt')) }} {{ ('2.0.0' is version('2', '==')) }}",
                &Map::new()
            )
            .unwrap(),
            json!("True False False")
        );
        let bad = t
            .render("{{ '1.0' is version('2', 'bogus') }}", &Map::new())
            .unwrap_err();
        assert!(bad.0.contains("Invalid operator type (bogus)"), "{bad}");
        let mixed = t
            .render("{{ '1.a' is version('1.2', '<') }}", &Map::new())
            .unwrap_err();
        assert!(
            mixed.0.contains(
                "Version comparison failed: '<' not supported between instances of 'str' and 'int'"
            ),
            "{mixed}"
        );
    }

    /// The default operator is `eq`, the operator can be given by name, a number compares as
    /// its text, and the two options no measured role writes are refused by name.
    #[test]
    fn version_takes_the_reference_s_arguments_and_refuses_the_rest() {
        let t = templar();
        assert_eq!(
            t.render(
                "{{ '1.02' is version('1.2') }} {{ '1.10.0' is version('1.9', operator='>=') }} {{ 20.04 is version('20.4', '==') }} {{ '1.0-rc1' is version('1.0', '>') }}",
                &Map::new()
            )
            .unwrap(),
            json!("True True True True")
        );
        for text in [
            "{{ '1' is version('2', '<', strict=true) }}",
            "{{ '1' is version('2', '<', version_type='semver') }}",
        ] {
            let err = t.render(text, &Map::new()).unwrap_err();
            assert!(err.0.contains("is not supported yet"), "{text}: {err}");
        }
        let empty = t
            .render("{{ '' is version('2', '<') }}", &Map::new())
            .unwrap_err();
        assert!(
            empty.0.contains("Input version value cannot be empty"),
            "{empty}"
        );
    }

    /// The four methods the measured roles call, through `pycompat`.
    ///
    /// What would make this red: the unknown-method callback not handing over to `pycompat`.
    #[test]
    fn python_methods_answer_on_values() {
        let t = templar();
        let mut m = Map::new();
        m.insert("s".into(), json!("a b,c"));
        m.insert("d".into(), json!({"k": 1}));
        assert_eq!(t.render("{{ s.split(' ')[0] }}", &m).unwrap(), json!("a"));
        assert_eq!(
            t.render("{{ s.startswith('a') }}", &m).unwrap(),
            json!(true)
        );
        assert_eq!(t.render("{{ s.find('b') }}", &m).unwrap(), json!(2));
        assert_eq!(t.render("{{ d.keys() | list }}", &m).unwrap(), json!(["k"]));
    }

    /// The `template` module's render reads the same methods and tests, since real templates
    /// call `.split()` too.
    ///
    /// What would make this red: `render_file` building an environment of its own instead of
    /// starting from the one the tests and the method callback are registered on.
    #[test]
    fn a_template_file_reads_the_methods_and_the_tests() {
        let mut m = Map::new();
        m.insert("s".into(), json!("a b"));
        m.insert("r".into(), json!({"changed": true}));
        let out = templar()
            .render_file(
                "{{ s.split()[1] }} {{ r is changed }} {{ '1.10' is version('1.9', '>') }}\n",
                &m,
                &super::super::FileRender::default(),
            )
            .unwrap();
        assert_eq!(out, "b True True\n");
    }

    /// A method neither `pycompat` nor this engine knows still fails by its name.
    #[test]
    fn a_method_nobody_knows_names_itself() {
        let err = templar()
            .render("{{ 'x'.casefold() }}", &Map::new())
            .unwrap_err();
        assert!(
            err.0.contains("method casefold is not available yet"),
            "{err}"
        );
    }
}
