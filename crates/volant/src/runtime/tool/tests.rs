// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use rstest::rstest;

fn words(command: &Command) -> Vec<String> {
    command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// The tool name, `sys.argv[0]` and the arguments follow the program text, untouched.
#[rstest]
#[case(true, &["-P", "-c"])]
#[case(false, &["-I", "-P", "-c"])]
fn the_arguments_reach_the_tool_unchanged(#[case] user_python: bool, #[case] head: &[&str]) {
    let args = [OsString::from("-h"), OsString::from("--no-such")];
    let built = command(
        "/py",
        user_python,
        "doc",
        Path::new("/bin/ansible-doc"),
        &args,
    );
    let got = words(&built);
    assert_eq!(got[..head.len()], *head);
    assert_eq!(
        got[head.len() + 1..],
        ["doc", "/bin/ansible-doc", "-h", "--no-such"]
    );
}

#[rstest]
#[case("ANSIBLE_CONFIG", true)]
#[case("TERM", true)]
#[case("PAGER", true)]
#[case("PYTHONPATH", false)]
#[case("PYTHONHOME", false)]
#[case("PIP_INDEX_URL", false)]
fn only_what_changes_the_interpreter_is_dropped(#[case] name: &str, #[case] kept: bool) {
    assert_eq!(forwarded(OsStr::new(name)), kept);
}

/// The managed child keeps the user's settings and sets the runtime's own; the user's
/// interpreter gets no change at all.
#[test]
fn only_the_managed_child_gets_the_runtime_environment() {
    let built = command("/py", false, "doc", Path::new("/x"), &[]);
    let env: std::collections::BTreeMap<_, _> = built.get_envs().collect();
    assert_eq!(env[OsStr::new("PYTHONNOUSERSITE")], Some(OsStr::new("1")));
    let own = command("/py", true, "doc", Path::new("/x"), &[]);
    assert_eq!(own.get_envs().count(), 0);
}
