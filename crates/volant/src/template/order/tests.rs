// SPDX-License-Identifier: GPL-3.0-or-later
use rstest::rstest;
use serde_json::{Map, json};

fn render(template: &str) -> Result<serde_json::Value, String> {
    let vars = json!({"s": "a", "one": 1, "nothing": null, "l": [1], "d": {}, "mixed": [1, "a"]});
    crate::testing::templar()
        .render(template, vars.as_object().unwrap_or(&Map::new()))
        .map_err(|e| e.0)
}

const NEG: &str = "('-340282366920938463463374607431768211457' | int)";
const POS: &str = "('340282366920938463463374607431768211457' | int)";

/// Measured on ansible-core 2.19.12: a wide integer orders against every number by value.
#[rstest]
#[case("{{ N < 0 }}", true)]
#[case("{{ N < -1 }}", true)]
#[case("{{ N > -1.5 }}", false)]
#[case("{{ N < P }}", true)]
#[case("{{ N >= ('-340282366920938463463374607431768211458' | int) }}", true)]
#[case("{{ P > 1e300 }}", false)]
#[case("{{ P <= 1e300 }}", true)]
#[case("{{ 0 < P <= 1e300 }}", true)]
#[case("{{ 0 < N <= 1e300 }}", false)]
#[case("{{ N == N }}", true)]
#[case("{{ N != 0 }}", true)]
#[case("{{ P in [P] }}", true)]
fn a_wide_integer_compares_exactly(#[case] template: &str, #[case] want: bool) {
    let template = template.replace('N', NEG).replace('P', POS);
    assert_eq!(render(&template), Ok(json!(want)), "{template}");
}

/// A chain keeps its operands and stops at the first false link, inside a macro and a loop too.
#[rstest]
#[case("{{ 1 < 2 < 3 }}", json!(true))]
#[case("{{ 3 > 2 > 2 }}", json!(false))]
#[case("{{ 2 < 1 < 'a' }}", json!(false))]
#[case("{{ 1 < 2 == 2 != 3 >= 3 }}", json!(true))]
#[case("{{ 1 in [1.0] }} {{ 1 not in [2] }}", json!("True True"))]
#[case("{{ ('nan' | float) < 1 }}", json!(false))]
#[case("{{ true == 1 }}", json!(true))]
#[case(
    "{% macro m(x) %}{{ 0 <= x < 3 }}{% endmacro %}{% for i in [1, 5] %}{{ m(i) }} {% endfor %}",
    json!("True False ")
)]
#[case("{% if 1 < 2 < 3 %}a{% endif %}{{ 'b' if 3 < 2 < 1 else 'c' }}", json!("ac"))]
fn chains_and_membership_keep_their_meaning(
    #[case] template: &str,
    #[case] want: serde_json::Value,
) {
    assert_eq!(render(template), Ok(want), "{template}");
}

/// Measured on ansible-core 2.19.12: Python's words for values it cannot order, the left operand
/// first. The operands are variables: minijinja folds a comparison of literals before it runs.
#[rstest]
#[case(
    "{{ s < 1 }}",
    "'<' not supported between instances of 'str' and 'int'"
)]
#[case(
    "{{ one >= 'a' }}",
    "'>=' not supported between instances of 'int' and 'str'"
)]
#[case(
    "{{ nothing < 1 }}",
    "'<' not supported between instances of 'NoneType' and 'int'"
)]
#[case(
    "{{ l < (one,) }}",
    "'<' not supported between instances of 'list' and 'tuple'"
)]
#[case(
    "{{ d < d }}",
    "'<' not supported between instances of 'dict' and 'dict'"
)]
#[case(
    "{{ mixed < [1, 2] }}",
    "'<' not supported between instances of 'str' and 'int'"
)]
#[case(
    "{{ 1 < 2 < s }}",
    "'<' not supported between instances of 'int' and 'str'"
)]
#[case(
    "{{ ['a', 1] | sort }}",
    "'<' not supported between instances of 'int' and 'str'"
)]
#[case(
    "{{ ['a', 1] | max }}",
    "'>' not supported between instances of 'int' and 'str'"
)]
#[case(
    "{% set l = ['a', 1] %}{% set _ = l.sort() %}{{ l }}",
    "'<' not supported between instances of 'int' and 'str'"
)]
fn what_python_cannot_order_is_refused_in_its_words(#[case] template: &str, #[case] said: &str) {
    let err = render(template).unwrap_err();
    assert!(err.contains(said), "{template}: {err}");
}
