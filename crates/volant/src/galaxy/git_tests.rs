// SPDX-License-Identifier: GPL-3.0-or-later

use rstest::rstest;

use super::git::{parse, redact};

/// `parse_scm` of ansible-core 2.19.12.
#[rstest]
#[case::root(
    "git+file:///work/repo/demo/.git",
    "*",
    "demo",
    "HEAD",
    "file:///work/repo/demo/.git",
    ""
)]
#[case::a_ref(
    "git+https://host/org/demo.git,v1.0.0",
    "*",
    "demo",
    "v1.0.0",
    "https://host/org/demo.git",
    ""
)]
#[case::the_version_field(
    "https://host/org/demo.git",
    "main",
    "demo",
    "main",
    "https://host/org/demo.git",
    ""
)]
#[case::a_subdirectory(
    "git+file:///work/mono/.git#/collections/demo",
    "*",
    "mono",
    "HEAD",
    "file:///work/mono/.git",
    "collections/demo"
)]
#[case::ssh(
    "git@host:org/demo.git",
    "*",
    "demo",
    "HEAD",
    "git@host:org/demo.git",
    ""
)]
fn a_git_pointer(
    #[case] source: &str,
    #[case] version: &str,
    #[case] name: &str,
    #[case] reference: &str,
    #[case] url: &str,
    #[case] fragment: &str,
) {
    assert_eq!(
        parse(source, version),
        (name.into(), reference.into(), url.into(), fragment.into())
    );
}

#[rstest]
#[case(
    "https://user:secret@host/org/demo.git",
    "https://user:********@host/org/demo.git"
)]
#[case("https://token@host/org/demo.git", "https://token@host/org/demo.git")]
#[case("file:///work/repo/.git", "file:///work/repo/.git")]
fn a_password_is_never_shown(#[case] url: &str, #[case] shown: &str) {
    assert_eq!(redact(url), shown);
}

/// nextest runs each test in a process of its own, so the empty `PATH` stays in this one.
#[cfg(unix)]
#[test]
fn a_missing_git_says_so() {
    use super::git::checkout;
    unsafe { std::env::set_var("PATH", "/nonexistent") };
    if std::path::Path::new("/usr/sbin/git").exists() || std::path::Path::new("/sbin/git").exists()
    {
        return;
    }
    let failure = checkout("git+https://user:secret@host/demo.git", "*", false, 0)
        .err()
        .expect("no git");
    assert_eq!(
        failure.summary(),
        "Could not find git executable to extract the collection from the Git repository `https://user:********@host/demo.git`: Failed to find required executable \"git\" in paths: /nonexistent:/sbin:/usr/sbin:/usr/local/sbin"
    );
}
