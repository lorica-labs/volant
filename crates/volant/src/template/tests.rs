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
                base: None,
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

/// `hostvars` answers for a controller spelling the inventory does not list, and an iteration,
/// a length and `in` over the listed hosts leave it out, as in ansible-core 2.19.12.
#[rstest::rstest]
#[case::implicit_is_read_by_name("{{ hostvars['localhost'].inventory_hostname }}", json!("localhost"))]
#[case::implicit_is_a_member("{{ 'localhost' in hostvars }}", json!(true))]
#[case::another_spelling_is_a_member("{{ '::1' in hostvars }}", json!(true))]
#[case::an_unknown_name_is_not("{{ 'nosuch' in hostvars }}", json!(false))]
#[case::iteration_lists_the_inventory("{{ hostvars | list }}", json!(["h1"]))]
#[case::length_counts_the_inventory("{{ hostvars | length }}", json!(1))]
#[case::keys_list_the_inventory("{{ hostvars.keys() | list }}", json!(["h1"]))]
fn hostvars_hides_the_unlisted_controller_from_iteration(
    #[case] source: &str,
    #[case] want: Value,
) {
    let t = templar();
    let hosts = Arc::new(vars(json!({
        "h1": {"inventory_hostname": "h1"},
        "localhost": {"inventory_hostname": "localhost"},
        "127.0.0.1": {"inventory_hostname": "localhost"},
        "::1": {"inventory_hostname": "localhost"}
    })));
    let shared = Arc::new(vars(json!({"groups": {"all": ["h1"]}})));
    let own = Map::new();
    let vars = Vars {
        map: &own,
        hostvars: Some(&hosts),
        shared: Some(&shared),
        facts: None,
        untrusted: None,
        untrusted_hosts: None,
        base: None,
    };
    assert_eq!(t.render(source, vars).unwrap(), want, "{source}");
}

/// The context hands out the shared view itself, builds it once, ranks inventory-wide names over
/// a fact and lists a shared name once.
#[test]
fn the_context_hands_out_the_shared_view_and_ranks_inventory_wide_names() {
    {
        let shared = Arc::new(vars(json!({"a": {"y": 1}, "b": {"y": 2}})));
        let empty = Map::new();
        let (ctx, _) = context_of(
            &templar(),
            Vars {
                map: &empty,
                hostvars: Some(&shared),
                shared: None,
                facts: None,
                untrusted: None,
                untrusted_hosts: None,
                base: None,
            },
        );
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
            base: None,
        };
        assert_eq!(
            t.render("{{ groups['web'] }}", vars).unwrap(),
            json!(["h1"])
        );
        // And the host's own names are still its own: the shared map answers for what it holds
        // and for nothing else.
        assert_eq!(t.render("{{ own }}", vars).unwrap(), json!(1));
    }
    // The host's own names whole in its map, or split with the base a loop's items share: a
    // name either layer holds is listed once.
    for (own, base) in [
        (
            json!({"groups": "a set_fact wrote this", "own": 1}),
            json!({}),
        ),
        (
            json!({"own": 1}),
            json!({"groups": "a set_fact wrote this"}),
        ),
        (
            json!({}),
            json!({"groups": "a set_fact wrote this", "own": 1}),
        ),
    ] {
        let shared = Arc::new(vars(json!({"groups": {"web": ["h1"]}})));
        let facts = Arc::new(vars(json!({"own": 2})));
        let own = vars(own);
        let base = Arc::new(vars(base));
        let (ctx, _) = context_of(
            &templar(),
            Vars {
                map: &own,
                hostvars: None,
                shared: Some(&shared),
                facts: Some(&facts),
                untrusted: None,
                untrusted_hosts: None,
                base: Some(&base),
            },
        );
        let mut names: Vec<String> = ctx
            .try_iter()
            .expect("the root context walks as a mapping")
            .map(|name| name.to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["groups", "own"], "{own:?} over {base:?}");
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
                base: None,
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
                base: None,
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

/// A host name from a managed host taints a listing of `hostvars`, and a nested template of
/// another host still renders.
#[test]
fn a_host_name_from_data_taints_the_hostvars_listing_only() {
    let t = templar();
    let view = Arc::new(vars(json!({
        "{{ 1 + 1 }}": {},
        "h2": {"x": "{{ 40 + 2 }}"},
    })));
    let empty = Map::new();
    let hosts = BTreeSet::from(["{{ 1 + 1 }}".to_string()]);
    let mut wrong = Vec::new();
    for (keys_data, text, want) in [
        (true, "{{ hostvars | list | first }}", json!("{{ 1 + 1 }}")),
        (
            true,
            "{{ hostvars.keys() | list | first }}",
            json!("{{ 1 + 1 }}"),
        ),
        (true, "{{ hostvars.h2.x }}", json!(42)),
        (false, "{{ hostvars | list | first }}", json!(2)),
    ] {
        let untrusted = BTreeSet::from([HOSTVARS_KEYS.to_string()]);
        let v = Vars {
            map: &empty,
            hostvars: Some(&view),
            shared: None,
            facts: None,
            untrusted: keys_data.then_some(&untrusted),
            untrusted_hosts: Some(&hosts),
            base: None,
        };
        let got = t.render(text, v);
        if got.as_ref() != Ok(&want) {
            wrong.push(format!("{keys_data} {text} -> {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
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
            base: None,
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

/// What `dict` says when it refuses, in CPython's words. The golden rows only record that the
/// reference failed; these are the texts it printed.
#[rstest::rstest]
#[case::too_long(
    "{{ dict([['a', 1, 2]]) }}",
    "dictionary update sequence element #0 has length 3; 2 is required"
)]
#[case::too_short(
    "{{ dict([['a']]) }}",
    "dictionary update sequence element #0 has length 1; 2 is required"
)]
#[case::second_element(
    "{{ dict([['a', 1], ['b']]) }}",
    "dictionary update sequence element #1 has length 1; 2 is required"
)]
#[case::text(
    "{{ dict('abcd') }}",
    "dictionary update sequence element #0 has length 1; 2 is required"
)]
#[case::mapping_of_one_key(
    "{{ dict([{'a': 1}]) }}",
    "dictionary update sequence element #0 has length 1; 2 is required"
)]
#[case::element_not_a_sequence(
    "{{ dict([1]) }}",
    "cannot convert dictionary update sequence element #0 to a sequence"
)]
#[case::unhashable_list("{{ dict([[[1], 2]]) }}", "unhashable type: 'list'")]
#[case::unhashable_mapping("{{ dict([[{'a': 1}, 2]]) }}", "unhashable type: 'dict'")]
#[case::number("{{ dict(5) }}", "'int' object is not iterable")]
#[case::float("{{ dict(1.5) }}", "'float' object is not iterable")]
#[case::boolean("{{ dict(true) }}", "'bool' object is not iterable")]
#[case::none("{{ dict(none) }}", "'NoneType' object is not iterable")]
#[case::none_key(
    "{{ dict([[none, 1]]) | to_json }}",
    "a dict key of type 'NoneType' is not supported"
)]
#[case::fractional_key(
    "{{ dict([[1.5, 1]]) }}",
    "a dict key of type 'float' is not supported"
)]
#[case::infinite_key(
    "{{ dict([[1e400, 1]]) }}",
    "a dict key of type 'float' is not supported"
)]
#[case::endless_iterable(
    "{{ dict(range(1000000000000)) }}",
    "cannot convert dictionary update sequence element #0 to a sequence"
)]
#[case::endless_element(
    "{{ dict([range(1000000000000)]) }}",
    "dictionary update sequence element #0 has length 1000000000000; 2 is required"
)]
#[case::two_arguments("{{ dict([], []) }}", "dict expected at most 1 argument, got 2")]
fn dict_refuses_in_the_words_of_python(#[case] text: &str, #[case] want: &str) {
    let err = templar().render(text, &Map::new()).unwrap_err();
    assert!(err.0.contains(want), "{text}: {err}");
}

#[test]
fn dict_of_an_undefined_value_is_an_undefined_read() {
    let t = templar();
    assert!(
        t.render("{{ dict(nope) }}", &Map::new())
            .unwrap_err()
            .is_undefined()
    );
    assert!(
        t.render("{{ dict([nope]) }}", &Map::new())
            .unwrap_err()
            .is_undefined()
    );
}

/// Integers beyond 64 bits are separate keys, and one that is the same number as another spelled
/// differently is not: `1`, `1.0` and `true` are one key.
#[test]
fn dict_keeps_integer_keys_beyond_64_bits_apart() {
    let t = templar();
    let length = |text: &str| t.render(text, &Map::new()).unwrap();
    assert_eq!(
        length("{{ dict([[18446744073709551616, 'a'], [18446744073709551617, 'b']]) | length }}"),
        json!(2)
    );
    assert_eq!(
        length("{{ dict([[18446744073709551616, 'a'], [18446744073709551616, 'b']]) | length }}"),
        json!(1)
    );
    assert_eq!(
        length("{{ dict([[1, 'a'], [1.0, 'b'], [true, 'c']]) | length }}"),
        json!(1)
    );
}

/// An iterable longer than the limit is refused, and one at the limit is not.
#[test]
fn dict_takes_a_bounded_number_of_pairs_from_one_iterable() {
    let pairs = |n: i64| -> minijinja::Value {
        minijinja::Value::from_iter((0..n).map(|i| minijinja::Value::from_iter([i, i])))
    };
    let at_limit = globals::build(vec![pairs(3)], 3).unwrap();
    assert_eq!(at_limit.len(), Some(3));
    let err = globals::build(vec![pairs(4)], 3).unwrap_err();
    assert!(
        err.to_string()
            .contains("dict() takes at most 3 pairs from one iterable"),
        "{err}"
    );
}

/// A `raw` block or a string constant writes text the reference renders once. Rows of
/// `(template, result)`, rendered as written and then as the value of a variable.
#[test]
fn a_raw_block_is_emitted_once() {
    let x = json!({"x": "val"});
    let rows = [
        ("{% raw %}{{ x }}{% endraw %}", "{{ x }}"),
        ("a{% raw %}{{ x }}{% endraw %}b{{ x }}", "a{{ x }}bval"),
        ("{%- raw -%} {{ x }} {%- endraw -%}", "{{ x }}"),
        ("{% raw %}{% if x %}{% endraw %}", "{% if x %}"),
        ("{% raw %}{# c #}{% endraw %}", "{# c #}"),
        ("{{ '{{ x }}' }}", "{{ x }}"),
        ("{{ '{{' }} x {{ '}}' }}", "{{ x }}"),
    ];
    for (text, want) in rows {
        let t = templar();
        let map = vars(x.clone());
        assert_eq!(t.render(text, &map), Ok(json!(want)), "{text}");
        assert_eq!(
            t.render_value(&json!({"k": [text]}), &map),
            Ok(json!({"k": [want]})),
            "{text} in a mapping"
        );
        // A variable holding it, resolved once and then again, as every task and every loop item
        // does with the map it is given.
        let mut map = vars(json!({"x": "val", "v": text, "w": "{{ v }}"}));
        let mut data = BTreeSet::new();
        for pass in 0..3 {
            let (resolved, untrusted) = t.resolve_vars_tainted(Vars {
                untrusted: Some(&data),
                ..Vars::from(&map)
            });
            map = resolved;
            data = untrusted;
            assert_eq!(map["v"], json!(want), "{text}, resolution {pass}");
            assert_eq!(
                t.render(
                    "{{ v }}",
                    Vars {
                        untrusted: Some(&data),
                        ..Vars::from(&map)
                    }
                ),
                Ok(json!(want)),
                "{text} read through a variable, resolution {pass}"
            );
        }
        assert_eq!(map["w"], json!(want), "{text} read by another variable");
    }
    // A tag a string constant writes is text too, which a chain of variables cannot tell apart.
    let t = templar();
    let map = vars(json!({"x": "val"}));
    assert_eq!(
        t.render("{{ '{% raw %}' }}{{ x }}{{ '{% endraw %}' }}", &map),
        Ok(json!("{% raw %}val{% endraw %}"))
    );
    // A list an expression builds keeps the text of its members.
    let none = Map::new();
    let list = "{{ ['{% raw %}{{ x }}{% endraw %}'] }}";
    let (items, data) = t.render_value_tainted(&json!(list), &none).unwrap();
    assert_eq!(items, json!(["{% raw %}{{ x }}{% endraw %}"]));
    assert!(data, "the items are not rendered again");
    // A variable that chains to a variable holding it is still read to its end.
    let map = vars(json!({"x": "val", "a": "{{ b }}", "b": "{% raw %}{{ x }}{% endraw %}"}));
    assert_eq!(t.render("{{ a }}", &map), Ok(json!("{{ x }}")));
}

/// A raw block does not make text from a managed host evaluate: a name that holds text from a
/// host is read as it is, wrapped in a raw block or not, and its text is never rendered.
#[test]
fn a_raw_block_in_text_from_a_host_is_never_rendered() {
    let t = templar();
    let payload = "{% raw %}{{ lookup('pipe', 'false') }}{% endraw %}{{ lookup('pipe', 'false') }}";
    let map = vars(json!({"r": payload, "x": "val"}));
    let host = BTreeSet::from(["r".to_string()]);
    let with = Vars {
        untrusted: Some(&host),
        ..Vars::from(&map)
    };
    for text in [
        "{{ r }}",
        "{% raw %}{{ r }}{% endraw %}{{ r }}",
        "{{ '{% raw %}' }}{{ r }}",
    ] {
        let want = if text.starts_with("{% raw") {
            format!("{{{{ r }}}}{payload}")
        } else if text.starts_with("{{ '{%") {
            format!("{{% raw %}}{payload}")
        } else {
            payload.to_string()
        };
        assert_eq!(t.render(text, with), Ok(json!(want)), "{text}");
    }
    let (resolved, data) = t.resolve_vars_tainted(Vars {
        map: &vars(json!({"r": payload, "a": "{{ r }}", "b": "{% raw %}{{ r }}{% endraw %}"})),
        untrusted: Some(&host),
        ..Vars::from(&map)
    });
    assert_eq!(resolved["r"], json!(payload));
    assert_eq!(resolved["a"], json!(payload));
    assert!(
        data.contains("a"),
        "a name built from a host's text is data"
    );
}

/// What the reference renders: any string that holds `{{`, `{%` or `{#`, and no other brace.
#[test]
fn a_template_is_a_string_with_an_opening_tag() {
    for (text, want) in [
        ("{{ x }}", true),
        ("a {% if x %}b{% endif %}", true),
        ("{# note #}", true),
        ("${#list[@]}", true),
        ("x {", false),
        ("{ {", false),
        ("{x} {y}", false),
        ("#{ {", false),
        ("", false),
    ] {
        assert_eq!(Templar::is_template(text), want, "{text:?}");
    }
}

/// A syntax error far along one line used to overflow the lexer's column counter, which panics in
/// a debug build. The render must give an error, for any length.
#[test]
fn a_very_long_broken_line_is_an_error_not_a_panic() {
    let t = templar();
    for unit in ["{#", "{{ ", "{% "] {
        for count in [32_768, 70_000] {
            let text = unit.repeat(count);
            assert!(t.render(&text, &Map::new()).is_err(), "{unit:?} x {count}");
        }
    }
}

/// Text an expression made, stored as a variable, is read back as it was: a `{#` at its end is
/// not an open comment to cut once more. A complete comment in a variable is still rendered.
#[test]
fn a_stored_string_ending_in_an_open_comment_is_kept() {
    let t = templar();
    let stored = vars(json!({"a": "x{#", "b": "{#", "c": "y{#-", "d": "z{#+", "e": "{# c #}w"}));
    let (resolved, _) = t.resolve_vars_tainted(&stored);
    assert_eq!(
        Value::Object(resolved),
        json!({"a": "x{#", "b": "{#", "c": "y{#-", "d": "z{#+", "e": "w"})
    );
    assert_eq!(t.render("{{ a }}", &stored).unwrap(), json!("x{#"));
}

/// A comment that opens at the end of a very long line is still an open comment, not a failure
/// of the panic guard.
#[test]
fn an_open_comment_ends_a_very_long_line() {
    let t = templar();
    let text = format!("{}{{#", "a".repeat(70_000));
    assert_eq!(
        t.render(&text, &Map::new()).unwrap(),
        json!("a".repeat(70_000))
    );
}

/// A variable whose value is a template is rendered when a template reads it, so a filter or an
/// operator sees the final value, in play vars, mappings and lists alike, as in ansible-core.
#[rstest::rstest]
#[case::arithmetic(json!({"c": 7, "b": "{{ c }}", "s": "{{ b | int + 1 }}"}), "{{ s }}", json!(8))]
#[case::int_filter(json!({"c": 7, "b": "{{ c }}"}), "{{ b | int }}", json!(7))]
#[case::plain_read(json!({"c": 7, "b": "{{ c }}"}), "{{ b }}", json!(7))]
#[case::depth_three(
    json!({"c": 7, "d1": "{{ c }}", "d2": "{{ d1 }}", "d3": "{{ d2 | int * 2 }}"}),
    "{{ d3 }}",
    json!(14)
)]
#[case::length(
    json!({"host": "example.org", "url": "http://{{ host }}:8080/x"}),
    "{{ url | length }}",
    json!(25)
)]
#[case::regex_replace(
    json!({"host": "example.org", "url": "http://{{ host }}:8080/x"}),
    "{{ url | regex_replace('example', 'zz') }}",
    json!("http://zz.org:8080/x")
)]
#[case::b64encode(
    json!({"host": "example.org", "url": "http://{{ host }}:8080/x"}),
    "{{ url | b64encode }}",
    json!("aHR0cDovL2V4YW1wbGUub3JnOjgwODAveA==")
)]
#[case::list_member(
    json!({"host": "example.org", "l": ["{{ host }}"]}),
    "{{ l | map('upper') | list }}",
    json!(["EXAMPLE.ORG"])
)]
#[case::list_sum(
    json!({"c": 7, "l": ["{{ c }}", "{{ c | int + 1 }}"]}),
    "{{ l | map('int') | sum }}",
    json!(15)
)]
#[case::dict_member(
    json!({"c": 7, "d": {"k": "{{ c }}", "n": {"m": "{{ c }}"}}}),
    "{{ d.k | int + d.n.m | int }}",
    json!(14)
)]
#[case::dict_through_a_variable(
    json!({"c": 7, "d": {"k": "{{ c }}"}, "e": "{{ d }}"}),
    "{{ e.k | int + 1 }}",
    json!(8)
)]
#[case::member_nothing_reads(
    json!({"d": {"bad": "{{ missing }}", "ok": 1}}),
    "{{ d.ok }}",
    json!(1)
)]
#[case::default_on_a_nested_undefined(
    json!({"u": "{{ missing }}"}),
    "{{ u | default('dflt') }}",
    json!("dflt")
)]
#[case::defined_on_a_nested_undefined(json!({"u": "{{ missing }}"}), "{{ u is defined }}", json!(false))]
#[case::default_omit(
    json!({"omit": "__omit__", "u": "{{ missing | default(omit) }}"}),
    "{{ u == omit }}",
    json!(true)
)]
#[case::text_around(
    json!({"c": 7, "b": "{{ c }}", "t": "x-{{ b }}-y"}),
    "{{ t | upper }}",
    json!("X-7-Y")
)]
#[case::in_a_statement(
    json!({"c": 7, "b": "{{ c }}"}),
    "{% if b | int > 5 %}big{% else %}small{% endif %}",
    json!("big")
)]
#[case::each_read_alike(
    json!({"c": 7, "b": "{{ c }}"}),
    "{{ b | int }}-{{ b | int + 1 }}-{{ b }}",
    json!("7-8-7")
)]
fn a_variable_holding_a_template_is_rendered_when_it_is_read(
    #[case] map: Value,
    #[case] text: &str,
    #[case] want: Value,
) {
    let t = templar();
    assert_eq!(t.render(text, &vars(map)), Ok(want), "{text}");
}

/// The same variables through the pass that resolves a map ahead of a task, and through the
/// condition a task asks.
#[test]
fn a_map_resolves_in_dependency_order_and_a_condition_reads_it() {
    let t = templar();
    let map = vars(json!({
        "s": "{{ b | int + 1 }}", "b": "{{ c }}", "c": 7,
        "cond": "{{ port | int > 8000 }}", "port": "{{ 8080 }}",
    }));
    let (resolved, untrusted) = t.resolve_vars_tainted(&map);
    assert_eq!(resolved["s"], json!(8));
    assert_eq!(resolved["cond"], json!(true));
    assert!(untrusted.is_empty());
    assert_eq!(t.condition("cond", &map), Ok(true));
    assert_eq!(t.condition("{{ b | int == 7 }}", &map), Ok(true));
    assert_eq!(
        t.render_file("{{ s }}", &map, &FileRender::default()),
        Ok("8".to_string())
    );
}

/// A variable that reads itself, directly or through others, fails as the reference's does, even
/// behind `default`; a template that fails for another reason fails the read that needs it.
#[rstest::rstest]
#[case::itself(json!({"a": "{{ a }}"}), "{{ a }}", RECURSION)]
#[case::through_another(json!({"a": "{{ b }}", "b": "{{ a }}"}), "{{ a }}", RECURSION)]
#[case::through_a_filter(
    json!({"a": "{{ b | int + 1 }}", "b": "{{ a | int + 1 }}"}),
    "{{ a | length }}",
    RECURSION
)]
#[case::behind_default(json!({"a": "{{ a | default('x') }}"}), "{{ a }}", RECURSION)]
#[case::behind_default_at_the_read(json!({"a": "{{ a }}"}), "{{ a | default('x') }}", RECURSION)]
#[case::a_failure_that_is_not_undefined(
    json!({"a": "{{ 1 | nosuchfilter }}"}),
    "{{ a | default('x') }}",
    "nosuchfilter"
)]
fn a_variable_that_cannot_render_fails_the_read_that_needs_it(
    #[case] map: Value,
    #[case] text: &str,
    #[case] error: &str,
) {
    let t = templar();
    let err = t.render(text, &vars(map)).unwrap_err();
    assert!(err.0.contains(error), "{text}: {err}");
    assert!(!err.is_undefined(), "{text}: {err}");
}

#[test]
fn an_undefined_nested_variable_is_an_undefined_read_when_used() {
    let t = templar();
    let map = vars(json!({"u": "{{ missing }}", "v": "{{ u }}"}));
    for text in ["{{ u }}", "{{ v | upper }}", "{{ u | length }}"] {
        let err = t.render(text, &map).unwrap_err();
        assert!(err.is_undefined(), "{text}: {err}");
    }
}

/// What came from a managed host is data wherever it is read: through a variable, through two,
/// through a filter, and on a second read of the same name.
#[test]
fn text_from_a_host_is_data_through_a_nested_variable() {
    let t = templar();
    let untrusted: BTreeSet<String> = ["reg".to_string()].into();
    let map = vars(json!({
        "c": 7, "reg": "{{ c }}",
        "d": "{{ reg }}", "e": "{{ d | upper }}",
        "l": ["{{ reg }}"], "m": {"k": "{{ reg }}"},
    }));
    let vars = || Vars {
        map: &map,
        hostvars: None,
        shared: None,
        facts: None,
        untrusted: Some(&untrusted),
        untrusted_hosts: None,
        base: None,
    };
    for (text, want) in [
        ("{{ reg | length }}", json!(7)),
        ("{{ d | length }}", json!(7)),
        ("{{ d | upper }}", json!("{{ C }}")),
        ("{{ e }}", json!("{{ C }}")),
        ("{{ e | length }}", json!(7)),
        ("{{ l | map('length') | list }}", json!([7])),
        ("{{ m.k | length }}", json!(7)),
        // The second read comes from the cache and still marks the render as data.
        ("{{ (d | length) ~ (d | upper) }}", json!("7{{ C }}")),
        ("{{ (e | length) ~ (e | upper) }}", json!("7{{ C }}")),
    ] {
        assert_eq!(t.render(text, vars()), Ok(want), "{text}");
    }
    assert_eq!(t.condition("d | length == 7", vars()), Ok(true));
    let (resolved, soiled) = t.resolve_vars_tainted(vars());
    assert_eq!(resolved["e"], json!("{{ C }}"));
    for name in ["reg", "d", "e", "l", "m"] {
        assert!(soiled.contains(name), "{name}");
    }
    assert!(!soiled.contains("c"));
}

/// `!unsafe` text stays text through a variable that reads it, and a gathered fact is never
/// rendered when a variable reads it.
#[test]
fn unsafe_text_and_facts_are_data_through_a_nested_variable() {
    let t = templar();
    let map = vars(json!({
        "c": 7,
        "u": crate::yaml::unsafe_data(json!("{{ c }}")),
        "v": "{{ u }}",
        "w": "{{ f }}",
    }));
    let facts = Arc::new(vars(json!({"f": "{{ c }}"})));
    let untrusted: BTreeSet<String> = ["f".to_string()].into();
    let vars = Vars {
        map: &map,
        hostvars: None,
        shared: None,
        facts: Some(&facts),
        untrusted: Some(&untrusted),
        untrusted_hosts: None,
        base: None,
    };
    for (text, want) in [
        ("{{ u | length }}", json!(7)),
        ("{{ v | length }}", json!(7)),
        ("{{ v | upper }}", json!("{{ C }}")),
        ("{{ f | length }}", json!(7)),
        ("{{ w | length }}", json!(7)),
    ] {
        assert_eq!(t.render(text, vars), Ok(want), "{text}");
    }
}

/// A read renders a variable only when all it reads is author-written: a chain over inventory
/// names, shared lists, `hostvars` or a lookup by text stays as written, at any depth.
#[test]
fn a_nested_read_renders_only_variables_that_read_author_text() {
    let t = templar();
    let map = vars(json!({
        "__volant_with_terms": ["{{ 1 + 1 }}"],
        "gn": "{{ groups }}",
        "gk": "{{ gn | list }}",
        "gl": "{{ gk | last }}",
        "ph": "{{ ansible_play_hosts_all }}",
        "pl": "{{ ph | list | last }}",
        "lk": "{{ lookup('vars', 'gn') }}",
        "lp": "{{ lk | length }}",
        "la": "{% set f = lookup %}{{ f('vars', 'gn') }}",
        "lb": "{{ la | length }}",
        "dir": "/srv/{{ inventory_hostname }}",
        "inventory_hostname": "h1",
        "c": 7,
        "b": "{{ c }}",
        "s": "{{ b | int + 1 }}",
    }));
    let shared = Arc::new(vars(json!({
        "groups": {"all": ["h1"], "{{ 1 + 1 }}": ["h2"]},
        "ansible_play_hosts_all": ["h1", "{{ 1 + 1 }}"],
    })));
    let untrusted = BTreeSet::new();
    let vars = || Vars {
        map: &map,
        hostvars: None,
        shared: Some(&shared),
        facts: None,
        untrusted: Some(&untrusted),
        untrusted_hosts: None,
        base: None,
    };
    assert_eq!(
        t.render("{{ __volant_with_terms[0] | length }}", vars()),
        Ok(json!(11))
    );
    // Left as written, so a filter reads the text of the variable and nothing is evaluated.
    for (text, raw) in [
        ("{{ gk | length }}", "{{ gn | list }}"),
        ("{{ gl | length }}", "{{ gk | last }}"),
        ("{{ pl | length }}", "{{ ph | list | last }}"),
        ("{{ lp | length }}", "{{ lk | length }}"),
        ("{{ lb | length }}", "{{ la | length }}"),
    ] {
        assert_eq!(t.render(text, vars()), Ok(json!(raw.len())), "{text}");
    }
    let (resolved, _) = t.resolve_vars_tainted(vars());
    for name in ["gk", "gl", "pl"] {
        assert_ne!(resolved[name], json!(2), "{name}");
    }
    // Author-written names still chain, and a path built from the host's name is the playbook's.
    assert_eq!(resolved["s"], json!(8));
    assert_eq!(t.render("{{ s }}", vars()), Ok(json!(8)));
    assert_eq!(resolved["dir"], json!("/srv/h1"));
}

fn number(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// Integers wider than 64 bits keep every digit through a template, as Python's do; arithmetic
/// works up to 128 bits and a wider operand is refused by name.
#[test]
fn integers_of_any_width_keep_their_digits_through_templates() {
    let (u64_end, i128_end, past) = (
        "18446744073709551616",
        "170141183460469231731687303715884105727",
        "340282366920938463463374607431768211456",
    );
    let wide = format!("1{}", "0".repeat(400));
    let vars = vars(Value::Object(Map::from_iter([
        ("a".to_string(), number(u64_end)),
        ("b".to_string(), number(i128_end)),
        ("c".to_string(), number(past)),
        ("d".to_string(), number(&format!("-{wide}"))),
        ("list".to_string(), number(&format!("[{past}, 1.5, 7]"))),
    ])));
    let t = templar();
    for (text, want) in [
        ("{{ a }}", number(u64_end)),
        ("{{ c }}", number(past)),
        ("{# a note #}{{ c }}", number(past)),
        ("{% if true %}{{ c }}{% endif %}", number(past)),
        ("{{ d }}", number(&format!("-{wide}"))),
        ("{{ list }}", number(&format!("[{past}, 1.5, 7]"))),
        ("{{ [c, d] | last }}", number(&format!("-{wide}"))),
        ("{{ {'k': c} }}", number(&format!(r#"{{"k": {past}}}"#))),
        ("{{ c | string }}", json!(past)),
        ("{{ c ~ '' }}", json!(past)),
        ("{{ d | string | length }}", json!(402)),
        ("{{ a + 1 }}", number("18446744073709551617")),
        ("{{ a * 3 }}", number("55340232221128654848")),
        (
            "{{ b - 1 }}",
            number("170141183460469231731687303715884105726"),
        ),
        ("{{ c == c }}", json!(true)),
        ("{{ c < c }}", json!(false)),
        ("{{ d < c }}", json!(true)),
        ("{{ c > a }}", json!(true)),
        ("{{ c in list }}", json!(true)),
        ("{{ c | int == c }}", json!(true)),
        ("{{ c | to_json }}", json!(past)),
        (
            "{{ {'k': [d]} | to_json }}",
            json!(format!(r#"{{"k": [-{wide}]}}"#)),
        ),
        ("{{ c | type_debug }}", json!("int")),
        (
            "{{ c is integer and c is number and c is not float }}",
            json!(true),
        ),
        ("{{ (c | string | int) == c }}", json!(true)),
        (
            "{{ '340282366920938463463374607431768211457' | int > c }}",
            json!(true),
        ),
        ("{{ (list | to_json | from_json)[0] == c }}", json!(true)),
    ] {
        assert_eq!(t.render(text, &vars), Ok(want), "{text}");
    }
    let err = t.render("{{ c + 1 }}", &vars).unwrap_err();
    assert!(err.0.contains("wider than 128 bits"), "{err}");
}

const ISO: &str = r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d{6})?$";
const SPACED: &str = r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d(\.\d{6})?$";
const AT_SPACED: &str = r"^at \d{4}-\d\d-\d\d \d\d:\d\d:\d\d(\.\d{6})?$";

/// What `now()` and `lipsum()` print, checked by shape: the clock and the dice are not the point.
#[rstest::rstest]
#[case::bare("{{ now() }}", ISO)]
#[case::utc_positional("{{ now(true) }}", ISO)]
#[case::format("{{ now(utc=true, fmt='%Y-%m-%d') }}", r"^\d{4}-\d\d-\d\d$")]
#[case::format_positional("{{ now(false, '%H:%M') }}", r"^\d\d:\d\d$")]
#[case::empty_format("{{ now(fmt='') }}", ISO)]
#[case::none_format("{{ now(fmt=none) }}", ISO)]
#[case::unknown_directive("{{ now(fmt='%Q') }}", "^%Q$")]
#[case::strftime("{{ now().strftime('%Y%m%d') }}", r"^\d{8}$")]
#[case::strftime_flag("{{ now().strftime('%-d') }}", r"^\d{1,2}$")]
#[case::isoformat("{{ now().isoformat() }}", ISO)]
#[case::isoformat_seconds("{{ now().isoformat(' ', 'seconds') }}", r"^\S+ \S+$")]
#[case::in_text("at {{ now() }}", AT_SPACED)]
#[case::concatenated("{{ 'at ' ~ now() }}", AT_SPACED)]
#[case::string_filter("{{ now() | string }}", SPACED)]
#[case::type_name("{{ now() | type_debug }}", "^datetime$")]
#[case::lipsum_text("{{ lipsum(2, false, 1, 2) }}", r"^[A-Z][a-z]*\.\n\n[A-Z][a-z]*\.$")]
#[case::lipsum_html(
    "{{ lipsum(2, true, 1, 2) }}",
    r"^<p>[A-Z][a-z]*\.</p>\n<p>[A-Z][a-z]*\.</p>$"
)]
#[case::lipsum_html_by_default(
    "{{ lipsum(1, min=3, max=5) }}",
    r"^<p>[A-Z][a-z]*( [a-z]+){2,3}\.</p>$"
)]
#[case::lipsum_keywords("{{ lipsum(html=false, n=1, min=1, max=2) }}", r"^[A-Z][a-z]*\.$")]
#[case::lipsum_none("{{ lipsum(n=0) }}", "^$")]
#[case::lipsum_negative("{{ lipsum(-1) }}", "^$")]
fn now_and_lipsum_print_their_shape(#[case] text: &str, #[case] shape: &str) {
    let out = templar().render(text, &Map::new()).unwrap();
    let pattern = regex::Regex::new(shape).unwrap();
    assert!(
        out.as_str().is_some_and(|s| pattern.is_match(s)),
        "{text}: {out}"
    );
}

/// `now()` is a `datetime`: attributes, methods and comparisons answer, nothing else does.
#[rstest::rstest]
#[case::year("{{ now().year > 2000 }}", json!(true))]
#[case::month("{{ now().month in range(1, 13) }}", json!(true))]
#[case::item("{{ now()['month'] == now().month }}", json!(true))]
#[case::microsecond("{{ now().microsecond < 1000000 }}", json!(true))]
#[case::zone("{{ now().tzinfo is none }}", json!(true))]
#[case::weekday("{{ now().weekday() + 1 == now().isoweekday() }}", json!(true))]
#[case::timestamp("{{ now().timestamp() > 1700000000 }}", json!(true))]
#[case::earlier("{{ now() <= now() }}", json!(true))]
#[case::not_a_string("{{ now() is string }}", json!(false))]
#[case::to_json("{{ [now()] | to_json | length > 20 }}", json!(true))]
fn now_answers_like_a_datetime(#[case] text: &str, #[case] want: Value) {
    assert_eq!(templar().render(text, &Map::new()), Ok(want), "{text}");
}

/// What the reference says when a call is wrong. Python's own words, from ansible-core 2.19.12.
#[rstest::rstest]
#[case::too_many(
    "{{ now(1, 2, 3) }}",
    "_now() takes from 0 to 2 positional arguments but 3 were given"
)]
#[case::unknown_keyword("{{ now(foo=1) }}", "_now() got an unexpected keyword argument 'foo'")]
#[case::twice_utc(
    "{{ now(true, utc=false) }}",
    "_now() got multiple values for argument 'utc'"
)]
#[case::twice_fmt(
    "{{ now(true, '%Y', fmt='%Y') }}",
    "_now() got multiple values for argument 'fmt'"
)]
#[case::format_type("{{ now(fmt=5) }}", "strftime() argument 1 must be str, not int")]
#[case::method_arguments(
    "{{ now().strftime('%Y', 1) }}",
    "strftime() takes at most 1 argument (2 given)"
)]
#[case::method_missing(
    "{{ now().strftime() }}",
    "strftime() missing required argument 'format' (pos 1)"
)]
#[case::separator(
    "{{ now().isoformat('xx') }}",
    "isoformat() argument 1 must be a unicode character, not str"
)]
#[case::timespec("{{ now().isoformat('T', 'nope') }}", "Unknown timespec value")]
#[case::lipsum_text_count(
    "{{ lipsum('a') }}",
    "'str' object cannot be interpreted as an integer"
)]
#[case::lipsum_float_count(
    "{{ lipsum(1.5) }}",
    "'float' object cannot be interpreted as an integer"
)]
#[case::lipsum_float_min(
    "{{ lipsum(1, true, 1.5, 2) }}",
    "'float' object cannot be interpreted as an integer"
)]
#[case::lipsum_empty_range("{{ lipsum(1, true, 5, 3) }}", "empty range in randrange(5, 3)")]
#[case::lipsum_equal_bounds("{{ lipsum(1, true, 1, 1) }}", "empty range in randrange(1, 1)")]
#[case::lipsum_keyword(
    "{{ lipsum(foo=1) }}",
    "generate_lorem_ipsum() got an unexpected keyword argument 'foo'"
)]
#[case::lipsum_twice(
    "{{ lipsum(1, n=2) }}",
    "generate_lorem_ipsum() got multiple values for argument 'n'"
)]
#[case::lipsum_too_many(
    "{{ lipsum(1, false, 1, 2, 3) }}",
    "generate_lorem_ipsum() takes from 0 to 4 positional arguments but 5 were given"
)]
fn now_and_lipsum_refuse_in_the_words_of_python(#[case] text: &str, #[case] want: &str) {
    let err = templar().render(text, &Map::new()).unwrap_err();
    assert!(err.0.contains(want), "{text}: {err}");
}

/// `lipsum` makes `n` paragraphs of at least `min` and fewer than `max` words, each ending in a dot.
#[test]
fn lipsum_makes_paragraphs_within_its_bounds() {
    let t = templar();
    for _ in 0..30 {
        let out = t
            .render("{{ lipsum(4, false, 3, 9) }}", &Map::new())
            .unwrap();
        let paragraphs: Vec<&str> = out.as_str().unwrap().split("\n\n").collect();
        assert_eq!(paragraphs.len(), 4, "{out}");
        for paragraph in paragraphs {
            let words = paragraph.split(' ').count();
            assert!((3..9).contains(&words), "{words} words in {paragraph}");
            assert!(paragraph.ends_with('.'), "{paragraph}");
            assert!(paragraph.starts_with(char::is_uppercase), "{paragraph}");
        }
    }
    let default = t.render("{{ lipsum() }}", &Map::new()).unwrap();
    let default = default.as_str().unwrap();
    assert_eq!(default.matches("<p>").count(), 5, "{default}");
    assert_eq!(default.matches("</p>").count(), 5, "{default}");
}

/// `lipsum` is random: two calls do not give the same text.
#[test]
fn lipsum_differs_from_one_call_to_the_next() {
    let t = templar();
    let texts: BTreeSet<String> = (0..5)
        .map(|_| {
            t.render("{{ lipsum(1, false, 40, 100) }}", &Map::new())
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(texts.len() > 1, "{texts:?}");
}
