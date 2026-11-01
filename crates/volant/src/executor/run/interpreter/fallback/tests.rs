// SPDX-License-Identifier: GPL-3.0-or-later

use rstest::rstest;

use super::read_probe;

/// The three ends of a probe measured on ansible-core 2.19.12: a name found, none found, and a
/// line the shell refused (a name holding a quote), each with the warning it registers before
/// the usual one and whether the usual one follows.
#[rstest]
#[case::found("FOUND\n/usr/bin/python3\nENDFOUND\n", "/usr/bin/python3", None, true)]
#[case::none_found(
    "FOUND\nENDFOUND\n",
    "/usr/bin/python3",
    Some(
        "No python interpreters found for host 'testhost' (tried ['/usr/bin/does_not_exist', 'python9'])."
    ),
    false
)]
#[case::broken_line(
    "",
    "/usr/bin/python3",
    Some(
        "Unhandled error in Python interpreter discovery for host 'testhost': unexpected output from Python interpreter discovery"
    ),
    true
)]
fn a_probe_reads_as_the_reference_reads_it(
    #[case] stdout: &str,
    #[case] interpreter: &str,
    #[case] note: Option<&str>,
    #[case] announced: bool,
) {
    let list = ["/usr/bin/does_not_exist".to_string(), "python9".to_string()];
    let probed = read_probe("testhost", &list, stdout);
    assert_eq!(probed.interpreter, interpreter);
    assert_eq!(probed.note.as_ref().map(|n| n.0.as_str()), note);
    assert_eq!(probed.announced, announced);
}
