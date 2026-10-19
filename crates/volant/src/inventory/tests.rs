// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use rstest::rstest;
use serde_json::json;

const SAMPLE: &str = r#"
# hosts without a group land in "ungrouped"
lonely ansible_connection=local

[web]
web1 ansible_host=10.0.0.1 role="front end"
web2

[db]
db1

[web:vars]
http_port=80
tier=web

[prod:children]
web
db

[prod:vars]
tier=prod
env=prod
"#;

fn names(hosts: &[Host]) -> Vec<&str> {
    hosts.iter().map(|h| h.name.as_str()).collect()
}

const ALL_VARS: &str = "[all:vars]\nx=1\ny=1\n\n[web]\nweb1\n\n[web:vars]\nx=2\n";
// Both groups hold web1 at depth 1: "zeta" is applied after "alpha", so it wins.
const EQUAL_DEPTH: &str =
    "[zeta]\nweb1\n[alpha]\nweb1\n[zeta:vars]\nk=zeta\n[alpha:vars]\nk=alpha\n";

/// A host's variable comes from the layer that wins: its own line, the deepest group, then
/// the shallower ones, `all` last, equal depths in alphabetical order.
#[rstest]
#[case::own_line(SAMPLE, "web1", "ansible_host", json!("10.0.0.1"))]
#[case::quoted_value(SAMPLE, "web1", "role", json!("front end"))]
#[case::group_var(SAMPLE, "web1", "http_port", json!(80))]
#[case::group_var_second_host(SAMPLE, "web2", "http_port", json!(80))]
#[case::child_wins_over_parent(SAMPLE, "web1", "tier", json!("web"))]
#[case::parent_reaches_child(SAMPLE, "web1", "env", json!("prod"))]
#[case::parent_alone(SAMPLE, "db1", "tier", json!("prod"))]
#[case::all_loses_to_a_group(ALL_VARS, "web1", "x", json!(2))]
#[case::all_reaches_every_host(ALL_VARS, "web1", "y", json!(1))]
#[case::equal_depth_alphabetical(EQUAL_DEPTH, "web1", "k", json!("zeta"))]
#[case::defined_localhost_wins("localhost ansible_connection=ssh\n", "localhost", "ansible_connection", json!("ssh"))]
fn a_host_variable_comes_from_the_layer_that_wins(
    #[case] ini: &str,
    #[case] host: &str,
    #[case] key: &str,
    #[case] want: Value,
) {
    let inv = Inventory::parse_ini(ini).unwrap();
    let found = inv.resolve(host);
    assert_eq!(found.hosts[0].vars[key], want, "{host}.{key}");
}

/// The pattern language: groups, unions, exclusions, intersections, wildcards over hosts and
/// groups, subscripts, in inventory order.
#[rstest]
#[case::all("all", &["lonely", "web1", "web2", "db1"])]
#[case::group("prod", &["web1", "web2", "db1"])]
#[case::comma_union("db,web1", &["db1", "web1"])]
#[case::colon_union("web:db", &["web1", "web2", "db1"])]
#[case::unknown_term_is_skipped("nope,web2", &["web2"])]
#[case::exclusion("all:!web", &["lonely", "db1"])]
#[case::intersection("prod:&web", &["web1", "web2"])]
#[case::intersection_then_exclusion("prod:&web:!web2", &["web1"])]
#[case::bare_exclusion_starts_from_all("!web", &["lonely", "db1"])]
#[case::empty("", &[])]
#[case::comma_only(",", &[])]
#[case::colon_only(":", &[])]
#[case::host_wildcard("w*b1", &["web1"])]
#[case::group_wildcard("pro*", &["web1", "web2", "db1"])]
#[case::regex_host("~w.*1", &["web1"])]
#[case::regex_group("~pro", &["web1", "web2", "db1"])]
#[case::regex_is_anchored_at_the_start("~b1", &[])]
#[case::index("web[0]", &["web1"])]
#[case::negative_index("web[-1]", &["web2"])]
#[case::slice("web[0:1]", &["web1", "web2"])]
#[case::out_of_range_selects_nothing("web[12]", &[])]
#[case::end_is_the_largest_integer("web[0:9223372036854775807]", &["web1", "web2"])]
#[case::deprecated_end_is_the_largest_integer("web[1-9223372036854775807]", &["web2"])]
#[case::end_beyond_sixty_four_bits("web[0:99999999999999999999]", &["web1", "web2"])]
#[case::start_beyond_sixty_four_bits("web[99999999999999999999:]", &[])]
#[case::a_zero_end_is_one_index("web[1:0]", &["web2"])]
fn a_pattern_selects_in_inventory_order(#[case] pattern: &str, #[case] want: &[&str]) {
    let inv = Inventory::parse_ini(SAMPLE).unwrap();
    assert_eq!(names(&inv.resolve(pattern).hosts), want, "{pattern}");
}

/// The controller is an implicit local host outside `all`, under each of its three spellings.
#[test]
fn the_controller_is_an_implicit_local_host() {
    let inv = Inventory::empty();
    for name in LOCALHOST_NAMES {
        let res = inv.resolve(name);
        assert_eq!(res.hosts.len(), 1, "{name} matched nothing");
        assert_eq!(res.hosts[0].name, name);
        assert_eq!(res.hosts[0].vars["ansible_connection"], json!("local"));
        assert!(res.unmatched.is_empty(), "{name}: {:?}", res.unmatched);
    }
    assert!(
        inv.resolve("all").hosts.is_empty(),
        "implicit localhost is not part of all"
    );
}

/// An unlisted spelling of the controller means the first controller host the inventory defines,
/// as in ansible-core 2.19.12, or an implicit host of its own name when none is defined.
#[rstest]
#[case::none_defined("[web]\nweb1\n", "127.0.0.1", &["127.0.0.1"], json!(null))]
#[case::another_spelling("[web]\nweb1\nlocalhost foo=1\n", "127.0.0.1", &["localhost"], json!(1))]
#[case::the_first_defined("127.0.0.1 foo=1\n::1 foo=2\n", "localhost", &["127.0.0.1"], json!(1))]
#[case::listed_means_itself("localhost foo=1\n127.0.0.1 foo=2\n", "127.0.0.1", &["127.0.0.1"], json!(2))]
#[case::a_pattern_of_spellings("localhost\n", "::1,localhost", &["localhost"], json!(null))]
#[case::not_a_spelling("localhost\n", "web1", &[], json!(null))]
#[case::two_implicit_spellings_are_one_host("[web]\nweb1\n", "localhost,::1", &["localhost"], json!(null))]
#[case::the_first_spelling_names_it("[web]\nweb1\n", "::1,127.0.0.1", &["::1"], json!(null))]
#[case::an_exclusion_by_another_spelling("[web]\nweb1\n", "127.0.0.1:!localhost", &[], json!(null))]
fn an_unlisted_controller_spelling_is_the_inventorys_controller(
    #[case] ini: &str,
    #[case] pattern: &str,
    #[case] want: &[&str],
    #[case] foo: Value,
) {
    let inv = Inventory::parse_ini(ini).unwrap();
    let res = inv.resolve(pattern);
    assert_eq!(names(&res.hosts), want, "{pattern}");
    if let Some(host) = res.hosts.first() {
        assert_eq!(host.vars.get("foo").unwrap_or(&Value::Null), &foo);
    }
}

/// The terms of a pattern, as ansible-core 2.19.12's `split_host_pattern` cuts them: the
/// controller's `::1` and IPv6 addresses stay whole, a comma list keeps the colons in its terms.
#[rstest]
#[case::ipv6_loopback("::1", &["::1"])]
#[case::ipv6_address("fe80::1", &["fe80::1"])]
#[case::ipv6_with_port("[::1]:22", &["[::1]:22"])]
#[case::host_with_port("web:22", &["web:22"])]
#[case::colon_list("a:b:c:d", &["a", "b", "c", "d"])]
#[case::colon_list_with_operators("prod:&web:!web2", &["prod", "&web", "!web2"])]
#[case::a_subscript_keeps_its_colon("web[0:1]", &["web[0:1]"])]
#[case::a_subscript_in_a_list("web[0:1]:db", &["web[0:1]", "db"])]
#[case::an_unbalanced_bracket_is_dropped("web[1:db", &["web", "1", "db"])]
#[case::a_comma_list_keeps_colons("web:db,cache", &["web:db", "cache"])]
#[case::a_comma_ignores_brackets("a[,b[1]", &["a[", "b[1]"])]
#[case::a_leading_colon_pair_is_not_an_address("::1:web", &["1", "web"])]
#[case::an_operator_before_an_address("!::1", &["!", "1"])]
#[case::spaces_are_dropped(" web : db ", &["web", "db"])]
#[case::colon_alone(":", &[])]
#[case::empty("", &[])]
fn a_pattern_is_cut_into_terms_as_the_reference_cuts_it(
    #[case] pattern: &str,
    #[case] want: &[&str],
) {
    assert_eq!(split_terms(pattern), want, "{pattern:?}");
}

/// The shortcut that turns a pattern away before the address expressions are built never
/// disagrees with `parse_address`.
#[test]
fn the_address_shortcut_agrees_with_parse_address() {
    let samples = [
        "::1",
        "fe80::1",
        "[::1]:22",
        "web:22",
        "web",
        "web:db",
        "all:!web",
        "prod:&web",
        "a:b:c:d",
        "web[0:1]",
        "web[0:1]:db",
        "web[1:db",
        "web[a:c]",
        "10.0.0.[1:3]",
        "10.0.0.1:22",
        "dead:beef",
        "::",
        ":",
        "a:",
        ":22",
        "[::1]",
        "[a",
        "web[1]:22",
        "ab:cd:ef:01:23:45:67:89",
        "x:y:22",
        "web:!db:22",
        "1:2:3:4:5:6:7:8:9",
        "web 1",
        "::ffff:10.0.0.1",
        "web.example.com:8080",
    ];
    for sample in samples {
        assert_eq!(
            patterns::is_one_address(sample),
            ranges::parse_address(sample, true).is_ok(),
            "{sample:?}"
        );
    }
}

#[rstest]
#[case::unterminated_quote("[web]\nweb1 role=\"unterminated\n", 2)]
#[case::no_equals_sign("[web:vars]\nno_equals_sign\n", 2)]
#[case::empty_host_name("[web]\n\"\"\n", 2)]
#[case::unbalanced_header("[web\nweb1\n", 1)]
fn a_syntax_error_carries_its_line_number(#[case] ini: &str, #[case] line: usize) {
    assert_eq!(Inventory::parse_ini(ini).unwrap_err().line, line);
}

/// An integer of any width is an int, as in Python; the rest are Python literals when they parse
/// as such and text when they do not.
#[rstest]
#[case::i64_max("ok=9223372036854775807", "ok", json!(i64::MAX))]
#[case::past_i64("huge=9223372036854775808", "huge", json!(9_223_372_036_854_775_808_u64))]
#[case::past_u64("huge=123_456_789_012_345_678_901_234_567_890", "huge", json!(123_456_789_012_345_678_901_234_567_890_u128))]
#[case::below_i64("huge=-9223372036854775809", "huge", json!(-9_223_372_036_854_775_809_i128))]
#[case::negative_zero("zero=-0", "zero", json!(0))]
#[case::int("port=8080", "port", json!(8080))]
#[case::float("ratio=0.5", "ratio", json!(0.5))]
#[case::boolean("flag=True", "flag", json!(true))]
#[case::none("nothing=None", "nothing", json!(null))]
#[case::quoted("name='quoted text'", "name", json!("quoted text"))]
#[case::yes_is_text("word=yes", "word", json!("yes"))]
#[case::list("list=[1,2]", "list", json!([1, 2]))]
#[case::first_equals_splits("raw=a=b", "raw", json!("a=b"))]
fn an_ini_value_is_a_python_literal_when_it_parses_as_one(
    #[case] words: &str,
    #[case] key: &str,
    #[case] want: Value,
) {
    let inv = Inventory::parse_ini(&format!("h {words}\n")).unwrap();
    assert_eq!(inv.resolve("h").hosts[0].vars[key], want, "{words}");
}

/// Groups are listed; a circular group resolves, and a host inside the cycle reaches `all`.
#[test]
fn groups_are_listed_and_cycles_terminate() {
    let inv = Inventory::parse_ini(SAMPLE).unwrap();
    assert_eq!(inv.group_names_of("web1"), ["prod", "web"]);
    let groups = inv.groups();
    assert_eq!(groups["web"], ["web1", "web2"]);
    assert_eq!(groups["prod"], ["web1", "web2", "db1"]);
    assert_eq!(groups["all"], ["lonely", "web1", "web2", "db1"]);
    assert_eq!(groups["ungrouped"], ["lonely"]);

    let inv = Inventory::parse_ini("[a:children]\nb\n\n[b:children]\na\n").unwrap();
    let res = inv.resolve("a");
    assert!(res.hosts.is_empty() && res.unmatched.is_empty());
    let inv = Inventory::parse_ini("[a:children]\nb\n\n[b:children]\na\n\n[a]\ntrapped\n").unwrap();
    assert_eq!(names(&inv.resolve("all").hosts), ["trapped"]);
}

/// Unknown terms are reported, not fatal; a host sharing a group's name means the host and
/// warns.
#[test]
fn unmatched_terms_and_homonyms_are_reported() {
    let inv = Inventory::parse_ini(SAMPLE).unwrap();
    let res = inv.resolve("nope,web2");
    assert_eq!(res.unmatched, ["nope"]);
    let inv = Inventory::parse_ini("[same]\nsame\nother\n").unwrap();
    let res = inv.resolve("same");
    assert_eq!(names(&res.hosts), ["same"]);
    assert_eq!(res.warnings.len(), 1);
}

#[test]
fn glob_matching_follows_fnmatch() {
    assert!(
        glob_match("web*", "web12") && glob_match("web?", "web1") && !glob_match("web?", "web12")
    );
    assert!(glob_match("web[12]", "web2") && !glob_match("web[12]", "web3"));
    assert!(glob_match("web[!1]", "web2") && !glob_match("web[!1]", "web1"));
    assert!(glob_match("w[a-z]b1", "web1") && !glob_match("w[a-z]b1", "w3b1"));
    // Measured against CPython's fnmatch.fnmatchcase (ansible-core 2.19.12's bundled
    // 3.13.15): an unclosed `[`, an empty `[]`, and a bare `[!]` are all literal text.
    assert!(glob_match("web[", "web["));
    assert!(glob_match("a[]", "a[]") && !glob_match("a[]", "a"));
    assert!(glob_match("a[!]", "a[!]") && !glob_match("a[!]", "a!") && !glob_match("a[!]", "a]"));
    // Ten stars and a miss: the scan is O(pattern * text), not a recursion per split point.
    assert!(!glob_match("*a*a*a*a*a*a*a*a*a*a*b", &"a".repeat(40)));
}

/// The reference's own answer for a value ambiguous in Rust, taken from the inventory golden
/// rather than retyped, so the two never drift apart.
#[test]
fn ini_integers_follow_pythons_literal_grammar() {
    let expected: Value =
        serde_json::from_str(include_str!("../../tests/golden/expected_inventory.json")).unwrap();
    let web1 = &expected["hostvars"]["web1"];
    let inv = Inventory::load(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden/inventory.ini")
            .as_path(),
    )
    .unwrap();
    let vars = inv.host_with_vars("web1").vars;
    assert_eq!(
        vars["leading"], web1["leading"],
        "a leading zero stays text"
    );
    assert_eq!(
        vars["grouped"], web1["grouped"],
        "an underscore digit separator still parses"
    );
}
