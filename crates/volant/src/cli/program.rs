// SPDX-License-Identifier: GPL-3.0-or-later
//! The ansible-core program names the binary answers to, and the subcommand each one runs.

use std::ffi::OsStr;
use std::path::Path;

/// Every ansible-core 2.19.12 program and the `volant` subcommand it behaves as.
pub const TOOLS: [(&str, &str); 9] = [
    ("ansible", "adhoc"),
    ("ansible-config", "config"),
    ("ansible-console", "console"),
    ("ansible-doc", "doc"),
    ("ansible-galaxy", "galaxy"),
    ("ansible-inventory", "inventory"),
    ("ansible-playbook", "playbook"),
    ("ansible-pull", "pull"),
    ("ansible-vault", "vault"),
];

/// The subcommand a program started as `program` (`argv[0]`) behaves as. A name whose subcommand
/// does not exist yet, like any other name, is `None`.
#[must_use]
pub fn subcommand_for(program: &OsStr) -> Option<&'static str> {
    let name = Path::new(program).file_name()?.to_str()?;
    match name {
        "ansible" => Some("adhoc"),
        "ansible-inventory" => Some("inventory"),
        "ansible-playbook" => Some("playbook"),
        "ansible-vault" => Some("vault"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("ansible-inventory", Some("inventory"))]
    #[case("ansible-playbook", Some("playbook"))]
    #[case("ansible", Some("adhoc"))]
    #[case("/usr/bin/ansible", Some("adhoc"))]
    #[case("/usr/local/bin/ansible-playbook", Some("playbook"))]
    #[case("ansible-vault", Some("vault"))]
    #[case("volant", None)]
    #[case("volant-playbook", None)]
    #[case("ansible-test", None)]
    #[case("ansible-playbook2", None)]
    #[case("", None)]
    fn only_a_name_in_the_table_is_captured(#[case] name: &str, #[case] want: Option<&str>) {
        assert_eq!(subcommand_for(OsStr::new(name)), want);
    }

    #[test]
    fn every_row_of_the_match_is_a_row_of_the_table() {
        for (name, sub) in TOOLS {
            if let Some(got) = subcommand_for(OsStr::new(name)) {
                assert_eq!(got, sub, "{name}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_not_captured() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            subcommand_for(OsStr::from_bytes(b"ansible-play\xffbook")),
            None
        );
    }
}
