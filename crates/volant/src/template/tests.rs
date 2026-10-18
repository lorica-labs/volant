// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::bridge::Bridge;
use crate::testing::templar;
use context::Hostvars;
use minijinja::Environment;
use serde_json::json;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

fn vars(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

/// A single expression keeps its type, text renders to a string, values template recursively and
/// keys do not, variables resolve through chains, braces and file names do not change the answer.
#[test]
fn values_keep_their_type_and_text_renders_to_a_string() {
    {
        let t = templar();
        assert_eq!(t.render("{{ 1 + 1 }}", &Map::new()).unwrap(), json!(2));
        assert_eq!(
            t.render("{{ flag }}", &vars(json!({"flag": true})))
                .unwrap(),
            json!(true)
        );
        assert_eq!(
            t.render("{{ items }}", &vars(json!({"items": [1, 2]})))
                .unwrap(),
            json!([1, 2])
        );
        assert_eq!(
            t.render("n={{ 1 + 1 }}", &Map::new()).unwrap(),
            json!("n=2")
        );
        assert_eq!(
            t.render("{{ a }}{{ b }}", &vars(json!({"a": 1, "b": 2})))
                .unwrap(),
            json!("12")
        );
        assert_eq!(t.render("plain", &Map::new()).unwrap(), json!("plain"));
    }
    {
        let t = templar();
        let v = t
            .render_value(
                &json!({"{{ k }}": ["{{ x }}", {"n": "{{ x + 1 }}"}], "lit": 3}),
                &vars(json!({"k": "key", "x": 1})),
            )
            .unwrap();
        assert_eq!(v, json!({"{{ k }}": [1, {"n": 2}], "lit": 3}));
    }
    {
        let t = templar();
        let resolved = t
            .resolve_vars_tainted(&vars(
                json!({"a": "{{ b }}", "b": "{{ c }}!", "c": "x", "bad": "{{ missing }}"}),
            ))
            .0;
        assert_eq!(resolved["a"], json!("x!"));
        assert_eq!(resolved["c"], json!("x"));
        assert_eq!(resolved["bad"], json!("{{ missing }}"));
    }
    {
        let t = templar();
        let none = Map::new();
        assert_eq!(
            t.render("{{ {'a': {'x': 5}} }}", &none),
            Ok(json!({"a": {"x": 5}}))
        );
        assert_eq!(t.render("{{ '}}' }}", &none), Ok(json!("}}")));
        assert!(t.render("{{ 1 }}{{ 2 }}", &none).unwrap().is_string());
        assert!(
            t.render("{{ {'a': 1} }} {{ 2 }}", &none)
                .unwrap()
                .is_string()
        );
        assert_eq!(
            t.condition("{{ {'a': {'x': 5}} == {'a': {'x': 5}} }}", &none),
            Ok(true)
        );
    }
    {
        let t = templar();
        for name in ["index.html.j2", "server.xml", "a.htm.jinja2"] {
            let opts = FileRender {
                name: Some(name.into()),
                ..FileRender::default()
            };
            assert_eq!(
                t.render_file("{{ \"<b>\" }} &", &Map::new(), &opts),
                Ok("<b> &".to_string()),
                "{name}"
            );
        }
    }
}

/// An undefined variable is an error in Ansible's words and stays undefined through every filter
/// and test, except the ones made to see it; every minijinja builtin goes through the wrapper.
#[test]
fn an_undefined_input_stays_undefined_through_every_filter_and_test() {
    {
        let t = templar();
        let err = t.render("{{ missing }}", &Map::new()).unwrap_err();
        assert!(
            err.0
                .starts_with("The task includes an option with an undefined variable."),
            "{err}"
        );
        assert!(
            t.render("{{ conf.missing }}", &vars(json!({"conf": {}})))
                .is_err()
        );
    }
    {
        {
            let t = templar();
            let r = |text: String| t.render(&text, &Map::new());
            for filter in FILTERS.split("; ") {
                let err = r(format!("{{{{ nope | {filter} }}}}")).unwrap_err();
                assert!(err.is_undefined(), "{filter}: {err}");
                assert_eq!(
                    r(format!("{{{{ nope | {filter} | default('x') }}}}")),
                    Ok(json!("x")),
                    "{filter}"
                );
                assert_eq!(
                    r(format!("{{{{ nope | {filter} is defined }}}}")),
                    Ok(json!(false)),
                    "{filter}"
                );
            }
            for test in TESTS.split("; ") {
                let err = r(format!("{{{{ nope is {test} }}}}")).unwrap_err();
                assert!(err.is_undefined(), "{test}: {err}");
            }
        }
        {
            let t = templar();
            for (text, want) in [
                ("{{ nope | default('x') }}", json!("x")),
                ("{{ nope | d('x') }}", json!("x")),
                ("{{ nope | ansible.builtin.default('x') }}", json!("x")),
                ("{{ nope | type_debug }}", json!("UndefinedMarker")),
                ("{{ nope is defined }}", json!(false)),
                ("{{ nope is ansible.builtin.defined }}", json!(false)),
                ("{{ nope is undefined }}", json!(true)),
                (
                    "{{ [{'a': 1}, {}] | selectattr('a', 'defined') | list }}",
                    json!([{"a": 1}]),
                ),
            ] {
                assert_eq!(t.render(text, &Map::new()), Ok(want), "{text}");
            }
            let err = t.render("{{ nope | mandatory }}", &Map::new()).unwrap_err();
            assert!(!err.is_undefined(), "{err}");
            assert!(err.0.contains("Mandatory variable"), "{err}");
        }
        {
            let bare = format!("{:?}", Environment::new());
            let ours = format!("{:?}", templar().env());
            let names = |debug: &str, field: &str| -> Vec<String> {
                let start = debug.find(&format!("{field}: [")).expect(field) + field.len() + 3;
                let end = start + debug[start..].find(']').expect(field);
                debug[start..end]
                    .split(", ")
                    .map(|n| n.trim_matches('"').to_string())
                    .collect()
            };
            for field in ["tests", "filters"] {
                let registered = names(&ours, field);
                let builtins = names(&bare, field);
                assert!(builtins.len() > 10, "{field}: {builtins:?}");
                for name in builtins {
                    assert!(
                        registered.contains(&format!("ansible.builtin.{name}")),
                        "{field} {name} is not wrapped"
                    );
                }
            }
        }
    }
}

/// The five shapes a playbook reads `hostvars` with. They are listed here as one test so
/// the map and the shared object below can be held to the same five answers.
fn hostvars_cases() -> [(&'static str, Value); 5] {
    [
        (
            "{{ hostvars | dict2items | map(attribute='key') | sort | join(',') }}",
            json!("a,b"),
        ),
        ("{{ hostvars | list | sort | join(',') }}", json!("a,b")),
        ("{{ hostvars['a'].y }}", json!(1)),
        ("{{ 'b' in hostvars }}", json!(true)),
        ("{{ hostvars | length }}", json!(2)),
    ]
}

fn hostvars_map() -> Map<String, Value> {
    vars(json!({"hostvars": {"a": {"y": 1}, "b": {"y": 2}}}))
}

/// `hostvars` answers the five shapes a playbook reads it with, from a map, the shared view, and a
/// bare map without a view.
#[test]
fn hostvars_answers_alike_from_a_map_a_view_or_a_bare_map() {
    {
        let t = templar();
        let map = hostvars_map();
        for (text, want) in hostvars_cases() {
            assert_eq!(t.render(text, &map).unwrap(), want, "{text}");
        }
    }
    {
        let t = templar();
        let shared = Arc::new(vars(json!({"a": {"y": 1}, "b": {"y": 2}})));
        let empty = Map::new();
        for (text, want) in hostvars_cases() {
            let vars = Vars {
                map: &empty,
                hostvars: Some(&shared),
                shared: None,
                facts: None,
                untrusted: None,
                untrusted_hosts: None,
            };
            assert_eq!(t.render(text, vars).unwrap(), want, "{text}");
        }
    }
    {
        let t = templar();
        assert_eq!(
            t.render("{{ hostvars['a'].y }}", &hostvars_map()).unwrap(),
            json!(1)
        );
    }
}

/// The context hands out the shared view itself, builds it once, ranks inventory-wide names over
/// a fact and lists a shared name once.
#[test]
fn the_context_hands_out_the_shared_view_and_ranks_inventory_wide_names() {
    {
        let shared = Arc::new(vars(json!({"a": {"y": 1}, "b": {"y": 2}})));
        let empty = Map::new();
        let (ctx, _) = context_of(Vars {
            map: &empty,
            hostvars: Some(&shared),
            shared: None,
            facts: None,
            untrusted: None,
            untrusted_hosts: None,
        });
        let first = ctx
            .get_attr("hostvars")
            .expect("hostvars is in the context")
            .downcast_object::<Hostvars>()
            .expect("the shared object, not a copy of the map");
        assert!(Arc::ptr_eq(&first.hosts, &shared));
        let second = ctx
            .get_attr("hostvars")
            .unwrap()
            .downcast_object::<Hostvars>()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second), "the view is built once");
        drop(first);
        drop(second);
        drop(ctx);
        assert_eq!(
            Arc::strong_count(&shared),
            1,
            "and the context gives it back when it goes"
        );
    }
    {
        let t = templar();
        let shared = Arc::new(vars(json!({"groups": {"web": ["h1"]}})));
        let fact = vars(json!({"groups": "a set_fact wrote this", "own": 1}));
        let vars = Vars {
            map: &fact,
            hostvars: None,
            shared: Some(&shared),
            facts: None,
            untrusted: None,
            untrusted_hosts: None,
        };
        assert_eq!(
            t.render("{{ groups['web'] }}", vars).unwrap(),
            json!(["h1"])
        );
        // And the host's own names are still its own: the shared map answers for what it holds
        // and for nothing else.
        assert_eq!(t.render("{{ own }}", vars).unwrap(), json!(1));
    }
    {
        let shared = Arc::new(vars(json!({"groups": {"web": ["h1"]}})));
        let fact = vars(json!({"groups": "a set_fact wrote this", "own": 1}));
        let (ctx, _) = context_of(Vars {
            map: &fact,
            hostvars: None,
            shared: Some(&shared),
            facts: None,
            untrusted: None,
            untrusted_hosts: None,
        });
        let mut names: Vec<String> = ctx
            .try_iter()
            .expect("the root context walks as a mapping")
            .map(|name| name.to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["groups", "own"]);
    }
}

/// `extract` over `hostvars` finds each host's address (the k3s CIDR loop) and taints like a bare
/// read.
#[test]
fn extract_reads_hostvars_by_host_name_and_taints_like_a_bare_read() {
    {
        let t = templar();
        let loop_text = r"{{
      (
        groups[server_group] | default([])
        + groups[agent_group] | default([])
      )
      | map('extract', hostvars)
      | selectattr('ansible_facts.default_ipv4', 'defined')
      | map(attribute='ansible_facts.default_ipv4.address')
      | flatten | unique | list
    }}";
        let view = Arc::new(vars(json!({
            "h1": {"ansible_facts": {"default_ipv4": {"address": "192.0.2.1"}}},
            "h2": {"ansible_facts": {}},
            "h3": {"ansible_facts": {"default_ipv4": {"address": "192.0.2.3"}}},
        })));
        let shared = Arc::new(vars(
            json!({"groups": {"server": ["h1"], "agent": ["h2", "h3"]}}),
        ));
        for (server_group, want) in [
            ("server", json!(["192.0.2.1", "192.0.2.3"])),
            ("nosuch", json!(["192.0.2.3"])),
        ] {
            let map = vars(json!({"server_group": server_group, "agent_group": "agent"}));
            let v = Vars {
                map: &map,
                hostvars: Some(&view),
                shared: Some(&shared),
                facts: None,
                untrusted: None,
                untrusted_hosts: None,
            };
            assert_eq!(t.render(loop_text, v).unwrap(), want, "{server_group}");
        }
    }
    {
        let t = templar();
        let view = Arc::new(vars(json!({"h2": {"x": "{{ 1 + 1 }}"}})));
        let empty = Map::new();
        let hosts = BTreeSet::from(["h2".to_string()]);
        let mut wrong = Vec::new();
        for (untrusted_hosts, want) in [(Some(&hosts), json!("{{ 1 + 1 }}")), (None, json!(2))] {
            let v = Vars {
                map: &empty,
                hostvars: Some(&view),
                shared: None,
                facts: None,
                untrusted: None,
                untrusted_hosts,
            };
            for text in [
                "{{ hostvars['h2'].x }}",
                "{{ 'h2' | extract(hostvars, 'x') }}",
                "{{ ['h2'] | map('extract', hostvars, ['x']) | first }}",
            ] {
                let got = t.render(text, v);
                if got.as_ref() != Ok(&want) {
                    wrong.push(format!("{text} -> {got:?}"));
                }
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}

/// Conditions need a boolean and accept stray braces; a delimited one reads an author string
/// again and never a host one.
#[test]
fn conditions_need_a_boolean_and_never_evaluate_a_host_string() {
    {
        let t = templar();
        assert!(t.condition("x > 3", &vars(json!({"x": 5}))).unwrap());
        assert!(!t.condition("x > 3", &vars(json!({"x": 1}))).unwrap());
        assert!(t.condition("{{ x > 3 }}", &vars(json!({"x": 5}))).unwrap());
        let err = t.condition("'yes'", &Map::new()).unwrap_err();
        assert!(
            err.0.contains("Conditionals must have a boolean result"),
            "{err}"
        );
        assert!(t.condition("missing > 3", &Map::new()).is_err());
    }
    {
        let t = templar();
        let map = vars(json!({
            "healthy": "true",
            "cond_s": "1 == 1",
            "hello": "hello",
            "s": {"stdout": "1 == 1"},
        }));
        let view = Arc::new(vars(json!({"h2": {"x": "1 == 1"}})));
        assert!(t.condition("{{ healthy }}", &map).unwrap());
        assert!(t.condition("{{ cond_s }}", &map).unwrap());
        let boolean = "Conditionals must have a boolean result";
        let err = t.condition("{{ hello }}", &map).unwrap_err();
        assert!(err.0.contains(boolean), "{err}");
        // Bare, the string is the result and nothing is evaluated again.
        let err = t.condition("healthy", &map).unwrap_err();
        assert!(err.0.contains(boolean), "{err}");

        let registered = BTreeSet::from(["s".to_string()]);
        let hosts = BTreeSet::from(["h2".to_string()]);
        let from_host = Vars {
            map: &map,
            hostvars: Some(&view),
            shared: None,
            facts: None,
            untrusted: Some(&registered),
            untrusted_hosts: Some(&hosts),
        };
        for text in [
            "{{ s.stdout }}",
            "{{ hostvars['h2'].x }}",
            "{{ lookup('env', 'PATH') }}",
        ] {
            let err = t.condition(text, from_host).unwrap_err();
            assert_eq!(
                err.0, "Encountered untrusted template or expression.",
                "{text}"
            );
        }
        let err = t.condition("s.stdout", from_host).unwrap_err();
        assert!(err.0.contains(boolean), "{err}");
        // The author's strings stay author strings beside a host's.
        assert!(t.condition("{{ cond_s }}", from_host).unwrap());
    }
}

/// Every filter the engine registers, with the arguments it needs, minijinja's builtins
/// included, separated by `; `.
const FILTERS: &str = "bool; int; float; ternary(1, 2); combine({}); dict2items; \
    items2dict; to_json; to_nice_json; from_json; basename; dirname; split(','); \
    regex_replace('a', 'b'); regex_search('a'); regex_findall('a'); b64decode; b64encode; \
    comment; difference([1]); intersect([1]); union([1]); flatten; from_yaml; to_uuid; \
    quote; regex_escape; extract({}); ansible.utils.ipwrap; to_yaml; to_nice_yaml; safe; \
    escape; e; lower; upper; title; capitalize; replace('a', 'b'); length; count; dictsort; \
    items; reverse; trim; join(','); lines; round; abs; attr('a'); first; last; min; max; \
    sort; list; string; batch(2); slice(2); sum; indent; select; reject; selectattr('a'); \
    rejectattr('a'); map('upper'); groupby('a'); unique; chain([1]); zip([1]); pprint; \
    format; ansible.builtin.dict2items; ansible.builtin.length";

/// The same for tests.
const TESTS: &str = "truthy; falsy; match('a'); search('a'); regex('a'); contains(1); \
    changed; failed; succeeded; skipped; version('1', '>'); none; boolean; odd; even; \
    divisibleby(2); number; integer; int; float; string; sequence; iterable; mapping; \
    startingwith('a'); endingwith('a'); lower; upper; sameas(1); eq(1); equalto(1); ne(1); \
    lt(1); le(1); gt(1); ge(1); in([1]); true; false; filter; test; safe; escaped; \
    ansible.builtin.none";

/// A literal, argument, container, concatenation or arithmetic holding an undefined value is an
/// undefined read, never `null` or an empty string.
#[test]
#[expect(clippy::too_many_lines)]
fn a_value_holding_an_undefined_one_is_an_undefined_read_in_every_shape() {
    {
        let t = templar();
        let vars = vars(json!({"items_": [{"x": 1}, {"y": 2}], "items": [1]}));
        for text in [
            "{{ [nope | lower] }}",
            "{{ {'k': nope | int} }}",
            "{{ dict(k=nope | lower) }}",
            "{{ [1] + [nope | lower] }}",
            "{{ {'a': 1} | combine({'k': nope | lower}) }}",
            "{{ [1] | union(nope | list) }}",
            "{{ '%s' | format(nope | lower) }}",
            "{{ [nope | lower] | to_json }}",
            "{{ [nope | lower] | to_nice_json }}",
            "{{ [nope | lower] | to_nice_yaml }}",
            "{{ lookup('env', nope | lower) }}",
            "{{ lookup('vars', nope | lower, default='z') }}",
            "{{ lookup('vars', 'zz', default=nope | lower) }}",
            "{{ 'a' | regex_replace('a', nope | lower) }}",
            "{{ 'a' | regex_replace('a', 'b', ignorecase=nope | bool) }}",
            "{{ [1, 2] | join(nope | lower) }}",
            "{{ 'a' is match(nope | lower) }}",
            "{{ 1 is eq(nope) }}",
            "{{ 1 is ne(nope | lower) }}",
            "{{ [nope] }}",
            "{{ items_ | map(attribute='x') | list }}",
            "{{ items + [nope | lower] }}",
            "{{ {'k': nope} | dict2items }}",
            "{{ [{'key': 'a', 'value': nope}] | items2dict }}",
            "{{ [[nope]] | flatten }}",
            "{{ {'a': 1} | combine({'k': nope}) }}",
            "{{ {'k': nope} | default('x', true) }}",
            "{{ [nope] | first }}",
            "{{ true | ternary(nope, 'b') }}",
            "x {{ [nope | lower] }}",
            "x {{ {'k': nope} }}",
        ] {
            let err = t.render(text, &vars).expect_err(&format!("leaked: {text}"));
            assert!(err.is_undefined(), "{text}: {err}");
        }
    }
    {
        let t = templar();
        let vars = vars(json!({"hs": ["a", "b"], "hv": {"a": {"ip": "10.0.0.1"}, "b": {}}}));
        for text in [
            "{{ [1, nope] | join(',') }}",
            "{{ [1, nope] | string }}",
            "{{ [1, nope] | map('quote') | join(' ') }}",
            "{{ [1, nope] | quote }}",
            "{{ [1, nope] | sum }}",
            "{{ hs | map('extract', hv, 'ip') | map('regex_replace', '^(.*)$', 'https://\\1:6443') | join(',') }}",
        ] {
            let err = t.render(text, &vars).expect_err(&format!("leaked: {text}"));
            assert!(err.is_undefined(), "{text}: {err}");
        }
        // The two the reference answers, unchanged.
        assert_eq!(t.render("{{ [nope] | length }}", &vars), Ok(json!(1)));
        assert_eq!(t.render("{{ [1, nope] | first }}", &vars), Ok(json!(1)));
    }
    {
        let t = templar();
        let none = Map::new();
        for text in [
            "{{ 'x' ~ [1, nope] }}",
            "{{ 'x' ~ nope }}",
            "{{ 'x' ~ {'k': nope} }}",
            "{{ [1, nope] ~ 'x' }}",
            "pre {{ 'x' ~ [1, nope] }} post",
            "{% block b %}{{ 'x' ~ [1, nope] }}{% endblock %}",
            "{{ ('x' ~ [1, nope]) | default('d') }}",
            "{{ ('x' ~ nope) | default('d') }}",
            "{{ 'a' ~ 'b' ~ [1, nope] }}",
        ] {
            let err = t.render(text, &none).expect_err(&format!("leaked: {text}"));
            assert!(err.is_undefined(), "{text}: {err}");
        }
        let err = t
            .condition("'x' ~ [1, nope] == 'x'", &none)
            .expect_err("leaked: when: 'x' ~ [1, nope] == 'x'");
        assert!(err.is_undefined(), "{err}");
        let err = t
            .render_file("{{ 'x' ~ [1, nope] }}", &none, &FileRender::default())
            .expect_err("leaked: template file {{ 'x' ~ [1, nope] }}");
        assert!(err.is_undefined(), "{err}");
        for (text, want) in [
            ("{{ 'a' ~ 'b' }}", json!("ab")),
            ("{{ 'a' ~ 'b' ~ 'c' }}", json!("abc")),
            ("{{ 1 ~ 2 }}", json!("12")),
            ("{{ 'x' ~ [1, 2] }}", json!("x[1, 2]")),
            ("{{ ([1, nope] + [2]) | length }}", json!(3)),
        ] {
            assert_eq!(t.render(text, &none), Ok(want), "{text}");
        }
        assert_eq!(t.condition("'a' ~ 'b' == 'ab'", &none), Ok(true));
        // Variables, not literals: minijinja folds `'a' ~ 'b'` at compile time, with no call.
        let shadow = vars(json!({ "volant::concat": 1, "a": "a", "b": "b" }));
        assert_eq!(t.render("{{ a ~ b }}", &shadow), Ok(json!("ab")));
    }
    {
        let t = templar();
        let none = Map::new();
        for text in [
            "{{ nope | int + 1 }}",
            "{{ 2 * (nope | int) }}",
            "{{ nope | int - 1 }}",
        ] {
            let err = t.render(text, &none).unwrap_err();
            assert!(err.is_undefined(), "{text}: {err}");
            assert_eq!(err, t.render("{{ nope }}", &none).unwrap_err(), "{text}");
        }
        let err = t.render("{{ 'a' - 1 }}", &none).unwrap_err();
        assert!(!err.is_undefined(), "{err}");
    }
}

/// The idioms roles write around undefined values, each measured on ansible-core 2.19.12.
#[test]
fn the_idioms_around_undefined_values_answer_as_the_reference() {
    let t = templar();
    let omit = crate::vars::omit_token();
    let vars = vars(json!({
        "items_": [{"x": 1}, {"y": 2}], "item": {"y": 2}, "s": "a,b", "y": "3", "omit": omit,
    }));
    for (text, want) in [
        ("{{ nope | default(omit) }}", json!(omit)),
        ("{{ item.x | default(omit) }}", json!(omit)),
        ("{{ (nope | default({})).get('k') }}", json!(null)),
        ("{{ (nope | default({})).k | default('z') }}", json!("z")),
        ("{{ nope is defined and nope | bool }}", json!(false)),
        (
            "{{ lookup('env', 'VOLANT_NOPE_X') | default('d', true) }}",
            json!("d"),
        ),
        ("{{ nope | d(y) | int }}", json!(3)),
        ("{{ false and nope | bool }}", json!(false)),
        ("{{ true or nope | bool }}", json!(true)),
        ("{{ true | ternary('a', nope) }}", json!("a")),
        ("{{ false | ternary(nope, 'b') }}", json!("b")),
        ("{{ true | ternary('a', nope | lower) }}", json!("a")),
        ("{{ 'x' | default(nope) }}", json!("x")),
        ("{{ 'x' | default(nope | lower) }}", json!("x")),
        ("{{ [nope] | type_debug }}", json!("list")),
        ("{{ [nope] | length }}", json!(1)),
        ("{{ [1, nope] | first }}", json!(1)),
        ("x {{ 'a' if false }}", json!("x ")),
        ("{{ [nope | lower] | length }}", json!(1)),
        ("{{ {'k': nope} | length }}", json!(1)),
        ("{{ nope | default([]) | length }}", json!(0)),
        (
            "{{ items_ | selectattr('x', 'defined') | list }}",
            json!([{"x": 1}]),
        ),
        (
            "{{ items_ | map(attribute='x') | select('defined') | list }}",
            json!([1]),
        ),
        ("{{ nope is defined and nope | length > 0 }}", json!(false)),
        (
            "{{ nope is not defined or nope | length == 0 }}",
            json!(true),
        ),
        ("{{ nope | default('') | length }}", json!(0)),
        ("{{ nope | d({}) | dict2items }}", json!([])),
        (
            "{{ items_ | map(attribute='x', default=0) | list }}",
            json!([1, 0]),
        ),
        ("{{ (nope is undefined) | ternary('u', 'd') }}", json!("u")),
        (
            "{{ items_ | selectattr('x', 'undefined') | list }}",
            json!([{"y": 2}]),
        ),
        (
            "{{ items_ | rejectattr('x', 'undefined') | list }}",
            json!([{"x": 1}]),
        ),
        ("{{ nope | default(false) | bool }}", json!(false)),
        ("{{ (nope | default([])) + [1] }}", json!([1])),
        ("{{ {'a': nope | default('z')} }}", json!({"a": "z"})),
        ("{{ lookup('vars', 'nope', default='z') }}", json!("z")),
        ("{{ s.split(',') | map('trim') | list }}", json!(["a", "b"])),
        (
            "{{ items_ | map(attribute='x') | select('defined') | map('string') | join(',') }}",
            json!("1"),
        ),
    ] {
        assert_eq!(t.render(text, &vars), Ok(want), "{text}");
    }
}

/// A bridge whose helper `start` gives, and the count of its starts.
fn bridged_templar(
    start: fn() -> anyhow::Result<crate::python::PythonBuilder>,
) -> (Templar, Arc<std::sync::atomic::AtomicUsize>) {
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&starts);
    let bridge = Bridge::with_start(Box::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        start()
    }));
    (
        Templar::with_bridge(PathBuf::from("."), Some(bridge)),
        starts,
    )
}

fn no_helper() -> anyhow::Result<crate::python::PythonBuilder> {
    anyhow::bail!("no ansible-core here")
}

fn dying_helper() -> anyhow::Result<crate::python::PythonBuilder> {
    crate::python::PythonBuilder::spawn("python3", &["-c", "import sys; sys.exit(3)"], &[])
}

/// A stand-in helper answering every call as `argv[1]` says: `not_found`, `failed`, or the
/// call's own arguments.
const STAND_IN: &str = r#"
import json, struct, sys
mode = sys.argv[1]
while True:
    head = sys.stdin.buffer.read(4)
    if len(head) < 4:
        break
    request = json.loads(sys.stdin.buffer.read(struct.unpack(">I", head)[0]))
    answer = {"id": request["id"]}
    if mode == "not_found":
        answer["error"] = {"kind": "not_found", "message": "The lookup plugin 'x.y' was not found."}
    elif mode == "failed":
        answer["error"] = {"kind": "plugin", "message": "The filter plugin 'x.y' failed: boom"}
    else:
        answer["value"] = request["call"]["args"]
    raw = json.dumps(answer).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(raw)) + raw)
    sys.stdout.buffer.flush()
"#;

fn empty_helper() -> anyhow::Result<crate::python::PythonBuilder> {
    crate::python::PythonBuilder::spawn("python3", &["-c", STAND_IN, "not_found"], &[])
}

fn failing_helper() -> anyhow::Result<crate::python::PythonBuilder> {
    crate::python::PythonBuilder::spawn("python3", &["-c", STAND_IN, "failed"], &[])
}

fn echo_helper() -> anyhow::Result<crate::python::PythonBuilder> {
    crate::python::PythonBuilder::spawn("python3", &["-c", STAND_IN, "echo"], &[])
}

/// What the helper answers reaches the template: a value, a plugin's own failure, a name it
/// does not know.
#[rstest::rstest]
#[case::filter_value(echo_helper, "{{ 'a' | x.y('b', k=1) }}", Ok(json!(["a", "b"])))]
#[case::test_value(echo_helper, "{{ 'a' is x.y }}", Ok(json!(true)))]
#[case::lookup_value(echo_helper, "{{ lookup('x.y', 't') }}", Ok(json!(["t"])))]
#[case::query_value(echo_helper, "{{ q('x.y', 't', wantlist=False) }}", Ok(json!(["t"])))]
#[case::undefined_filter_input(echo_helper, "{{ nope | x.y | default(1) }}", Ok(json!(1)))]
#[case::plugin_failure(
    failing_helper,
    "{{ 'a' | x.y }}",
    Err("The filter plugin 'x.y' failed: boom")
)]
#[case::unknown_test(
    empty_helper,
    "{{ 'a' is x.y }}",
    Err("unknown test: test x.y is unknown (in <expression>:1)")
)]
#[case::unknown_lookup(
    empty_helper,
    "{{ lookup('x.y') }}",
    Err("The lookup plugin 'x.y' was not found.")
)]
#[case::jinja_object_in_a_lookup(
    echo_helper,
    "{{ lookup('x.y', range) }}",
    Err("lookup 'x.y' was given a Jinja object, which cannot be sent to the Python helper")
)]
#[case::jinja_object_in_a_keyword(
    echo_helper,
    "{{ 'a' is x.y(k=range) }}",
    Err("test 'x.y' was given a Jinja object, which cannot be sent to the Python helper")
)]
fn what_the_helper_answers_reaches_the_template(
    #[case] helper: fn() -> anyhow::Result<crate::python::PythonBuilder>,
    #[case] text: &str,
    #[case] want: Result<Value, &str>,
) {
    let (t, _) = bridged_templar(helper);
    assert_eq!(
        t.render(text, &Map::new()),
        want.map_err(|e| TemplateError(e.into())),
        "{text}"
    );
}

#[test]
fn an_undefined_value_given_to_a_test_is_an_undefined_read() {
    let (t, _) = bridged_templar(echo_helper);
    let err = t.render("{{ nope is x.y }}", &Map::new()).unwrap_err();
    assert!(err.is_undefined(), "{err}");
}

/// ansible-core resolves a filter's name before it looks at the value: an undefined value
/// given to a filter nobody has fails as unknown, not as undefined.
#[test]
fn an_undefined_value_given_to_a_missing_filter_fails_as_unknown() {
    let (t, _) = bridged_templar(empty_helper);
    assert_eq!(
        t.render("{{ nope | x.y | default('d') }}", &Map::new()),
        Err(TemplateError(
            "unknown filter: filter x.y is unknown (in <expression>:1)".into()
        ))
    );
}

/// A native name never starts the helper; what the engine lacks goes to it, or fails naming it.
#[rstest::rstest]
#[case::k3s_ipwrap_stays_native("{{ '::1' | ansible.utils.ipwrap }}", Ok(json!("[::1]")), 0)]
#[case::qualified_lookup_stays_native("{{ lookup('ansible.builtin.vars', 'x') }}", Ok(json!(1)), 0)]
#[case::qualified_builtin_stays_native("{{ [1] | ansible.builtin.length }}", Ok(json!(1)), 0)]
#[case::native_test("{{ 'a' is ansible.builtin.string }}", Ok(json!(true)), 0)]
#[case::native_lookup("{{ query('vars', 'x') }}", Ok(json!([1])), 0)]
#[case::unvault_stays_native("{{ 'x' | unvault('s') }}", Ok(json!("x")), 0)]
#[case::vault_test_stays_native("{{ 'x' is vault_encrypted }}", Ok(json!(false)), 0)]
#[case::undefined_input_still_resolves_the_name(
    "{{ nope | community.general.dict_kv('k') | default({}) }}",
    Ok(json!({})),
    1
)]
#[case::legacy_native_stays_native("{{ [1] | ansible.legacy.length }}", Ok(json!(1)), 0)]
#[case::filter_named_by_map(
    "{{ ['v'] | map('community.general.dict_kv', 'k') | list }}",
    Err(
        "filter 'community.general.dict_kv' needs the Python helper, which could not start: no ansible-core here"
    ),
    1
)]
#[case::jinja_object_is_refused(
    "{% macro m() %}{% endmacro %}{{ m | community.general.dict_kv('k') }}",
    Err(
        "filter 'community.general.dict_kv' was given a Jinja object, which cannot be sent to the Python helper"
    ),
    0
)]
#[case::a_filter(
    "{{ 'v' | community.general.dict_kv('k') }}",
    Err(
        "filter 'community.general.dict_kv' needs the Python helper, which could not start: no ansible-core here"
    ),
    1
)]
#[case::a_test(
    "{{ 'a' is ansible.builtin.abs }}",
    Err(
        "test 'ansible.builtin.abs' needs the Python helper, which could not start: no ansible-core here"
    ),
    1
)]
#[case::a_lookup(
    "{{ q('sequence', 'end=3') }}",
    Err("lookup 'sequence' needs the Python helper, which could not start: no ansible-core here"),
    1
)]
fn a_name_the_engine_lacks_goes_to_the_python_helper(
    #[case] text: &str,
    #[case] want: Result<Value, &str>,
    #[case] starts: usize,
) {
    let (t, started) = bridged_templar(no_helper);
    let vars = vars(json!({"x": 1}));
    assert_eq!(
        t.render(text, &vars),
        want.map_err(|e| TemplateError(e.into())),
        "{text}"
    );
    assert_eq!(started.load(Ordering::SeqCst), starts, "{text}");
}

#[test]
fn a_helper_that_dies_fails_the_render_and_every_later_one() {
    let (t, _) = bridged_templar(dying_helper);
    let text = "{{ 'v' | community.general.dict_kv('k') }}";
    let first = t.render(text, &Map::new()).unwrap_err();
    assert!(
        first.0.starts_with(
            "filter 'community.general.dict_kv' needs the Python helper, which failed: the Python helper exited"
        ),
        "{first}"
    );
    assert_eq!(t.render(text, &Map::new()), Err(first));
}

/// A `!vault` value is read when a task uses it and is text from then on; one that cannot be
/// read raises only where something renders it.
#[test]
fn a_vaulted_value_is_text_when_read_and_raises_only_when_used() {
    let cipher = crate::vault::encrypt(b"{{ x }} and more", b"pw", None).unwrap();
    let map = vars(json!({"x": "hello", "sv": crate::yaml::vault_data(&cipher)}));
    let t = templar();
    crate::vault::install(crate::vault::Secrets::new(vec![b"pw".to_vec()]));
    let (resolved, untrusted) = t.resolve_vars_tainted(&map);
    assert_eq!(resolved["sv"], json!("{{ x }} and more"));
    assert!(untrusted.contains("sv"));
    crate::vault::install(crate::vault::Secrets::new(vec![b"other".to_vec()]));
    let (resolved, _) = t.resolve_vars_tainted(&map);
    let err = t.render("{{ sv }}", &resolved).unwrap_err();
    assert_eq!(
        err.0,
        "Attempt to use undecryptable variable: Decryption failed (no vault secrets were found that could decrypt)."
    );
}

/// Inside text a container prints as Python's `repr` and a whole `none` prints nothing; an
/// undefined parent stays undefined until `default` decides; a final newline is kept.
#[test]
fn text_prints_containers_none_and_chained_undefined_as_the_reference_does() {
    let t = templar();
    let v = vars(json!({"l": ["a", null], "n": null, "d": {"k": true}, "nl": "x"}));
    for (text, want) in [
        (
            "{{ l }} {{ d }}[{{ n }}]",
            json!("['a', None] {'k': True}[]"),
        ),
        ("{{ n }}", json!(null)),
        ("{{ nope.a.b | default('z') }}", json!("z")),
        ("{{ d.no.sub | default('z') }}", json!("z")),
        ("{{ d['no']['sub'] is defined }}", json!(false)),
        ("{{ nl }}\n", json!("x\n")),
    ] {
        assert_eq!(t.render(text, &v).unwrap(), want, "{text}");
    }
    assert!(t.render("{{ nope.a }}", &v).is_err());
    assert!(t.render("x {{ d.no.sub }}", &v).is_err());
}
