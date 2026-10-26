// SPDX-License-Identifier: GPL-3.0-or-later
use super::help::columnize;
use super::session::is_module;
use rstest::rstest;

/// The expected texts are CPython's `cmd.Cmd.columnize` on the same names and width.
#[rstest]
#[case(&["a"], 79, "a\n")]
#[case(&["EOF", "become", "cd", "a_very_long_name_that_takes_room", "x"], 30,
    "EOF\nbecome\ncd\na_very_long_name_that_takes_room\nx\n")]
#[case(&["n00", "n01", "n02", "n03", "n04", "n05", "n06", "n07", "n08", "n09", "n10", "n11"], 20,
    "n00  n03  n06  n09\nn01  n04  n07  n10\nn02  n05  n08  n11\n")]
fn names_are_laid_out_as_cmd_lays_them_out(
    #[case] names: &[&str],
    #[case] width: usize,
    #[case] want: &str,
) {
    let names: Vec<String> = names.iter().map(|n| (*n).to_string()).collect();
    assert_eq!(columnize(&names, width), want);
}

#[rstest]
#[case("ping", true)]
#[case("ansible.builtin.ping", true)]
#[case("command", true)]
#[case("community.general.ufw", true)]
#[case("echo", false)]
#[case("ansible.builtin.nosuch", false)]
#[case("a..b", false)]
fn a_first_word_is_a_module_when_it_names_one(#[case] word: &str, #[case] want: bool) {
    assert_eq!(is_module(word), want);
}
