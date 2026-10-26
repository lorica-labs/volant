// SPDX-License-Identifier: GPL-3.0-or-later
use super::steps::repo_options;
use super::*;
use clap::Parser as _;
use rstest::rstest;

fn args(extra: &[&str]) -> PullArgs {
    PullArgs::parse_from([&["pull", "-U", "u"][..], extra].concat())
}

#[rstest]
#[case(&[], "name=u dest=/d depth=1")]
#[case(&["-C", "v1", "--full", "--clean"], "name=u dest=/d version=v1 force=yes")]
#[case(
    &["--accept-host-key", "--private-key", "/k", "--verify-commit", "--track-subs"],
    "name=u dest=/d accept_hostkey=yes key_file=/k verify_commit=yes track_submodules=yes depth=1"
)]
#[case(&["-m", "subversion", "-C", "7"], "repo=u dest=/d revision=7 export=yes")]
#[case(&["-m", "subversion", "--full"], "repo=u dest=/d")]
#[case(&["-m", "hg", "-C", "7", "--clean"], "repo=u dest=/d revision=7 force=yes")]
#[case(&["-m", "bzr", "-C", "7"], "name=u dest=/d version=7")]
fn the_module_gets_the_options_the_reference_writes(#[case] extra: &[&str], #[case] want: &str) {
    assert_eq!(repo_options(&args(extra), "u", Path::new("/d")), want);
}

#[test]
fn the_names_of_the_machine_are_given_once_each() {
    let names = host_names();
    let mut seen = names.all.clone();
    seen.dedup();
    assert_eq!(seen.len(), names.all.len());
    assert_eq!(names.all.first(), Some(&names.fqdn));
}
