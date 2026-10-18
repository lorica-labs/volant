// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use rstest::rstest;

#[rstest]
#[case::padding("web[01:03]", &["web01", "web02", "web03"])]
#[case::letters("db[a:c]", &["dba", "dbb", "dbc"])]
#[case::step_end_inclusive("n[1:10:3]", &["n1", "n4", "n7", "n10"])]
#[case::tail_and_two_ranges("r[1:2]-[a:b].x", &["r1-a.x", "r1-b.x", "r2-a.x", "r2-b.x"])]
#[case::empty_begin("h[:2]", &["h0", "h1", "h2"])]
#[case::plain("plain", &["plain"])]
fn a_range_expands_in_order(#[case] pattern: &str, #[case] want: &[&str]) {
    assert_eq!(expand(pattern).unwrap(), want);
}

#[rstest]
#[case::unclosed("[web", "not enough values to unpack (expected 3, got 2)")]
#[case::one_bound("h[1]", "host range must be begin:end or begin:end:step")]
#[case::no_end("h[1:]", "host range must specify end value")]
#[case::widths(
    "h[01:100]",
    "host range must specify equal-length begin and end formats"
)]
#[case::backwards("h[c:a]", "host range must have begin <= end")]
#[case::not_a_number("h[1:x]", "invalid literal for int() with base 10: 'x'")]
#[case::zero_letter_step("h[a:c:0]", "invalid literal for int() with base 10: 'a'")]
fn a_bad_range_is_refused_with_the_reference_s_text(#[case] pattern: &str, #[case] want: &str) {
    assert_eq!(expand(pattern).unwrap_err(), want);
}

#[rstest]
#[case::port("a:2222", true, Some(("a", Some(2222))))]
#[case::bracketed_ipv6("[::1]:22", false, Some(("::1", Some(22))))]
#[case::bare_ipv6("fe80::1", false, Some(("fe80::1", None)))]
#[case::ipv4("10.0.0.1:8", false, Some(("10.0.0.1", Some(8))))]
#[case::range_with_port("web[1:3]:22", true, Some(("web[1:3]", Some(22))))]
#[case::range_refused("web[1:3]", false, None)]
#[case::not_a_port("b:x", false, None)]
#[case::trailing_dash("bad-", false, None)]
fn an_address_splits_into_host_and_port(
    #[case] text: &str,
    #[case] ranges: bool,
    #[case] want: Option<(&str, Option<i64>)>,
) {
    let got = parse_address(text, ranges).ok();
    assert_eq!(got.as_ref().map(|(h, p)| (h.as_str(), *p)), want);
}
