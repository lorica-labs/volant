// SPDX-License-Identifier: GPL-3.0-or-later
//! The usage errors of `ansible`: the `usage:` block of the reference, then the error line, exit 2.
//! clap words its own errors differently, so these are printed by hand.

use clap::error::{ContextKind, ContextValue, ErrorKind};

/// What `ansible --help` prints in ansible-core 2.19.12. Its first paragraph is the `usage:`
/// block every usage error opens with; the rest is shown after an argparse error only.
const HELP: &str = include_str!("adhoc_help.txt");

/// The `usage:` block, one line per line, with its final newline.
fn usage() -> &'static str {
    &HELP[..HELP.find("\n\n").expect("the help has a usage paragraph") + 1]
}

/// The usage block and `ansible: error: message` on standard error, and exit code 2. The
/// complete help follows when `with_help` is set, after the single space line the reference
/// prints between them: it does that for the errors argparse raises, not for the ones the
/// command raises once it has parsed.
pub(super) fn refuse(message: &str, with_help: bool) -> i32 {
    eprintln!("{}ansible: error: {message}", usage());
    if with_help {
        eprint!(" \n{HELP}");
    }
    2
}

/// The reference's wording and exit code for the usage errors clap would otherwise word itself:
/// a missing pattern, and a `--forks` that is not a number. `None` for any other error, which
/// stays clap's.
#[must_use]
pub fn usage_error(err: &clap::Error) -> Option<i32> {
    let arg = match err.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(arg)) => arg.clone(),
        Some(ContextValue::Strings(args)) => args.join(" "),
        _ => return None,
    };
    match err.kind() {
        ErrorKind::MissingRequiredArgument if arg.contains("<PATTERN>") => Some(refuse(
            "the following arguments are required: pattern",
            true,
        )),
        ErrorKind::ValueValidation | ErrorKind::InvalidValue if arg.contains("--forks") => {
            let Some(ContextValue::String(value)) = err.get(ContextKind::InvalidValue) else {
                return None;
            };
            Some(refuse(
                &format!("argument -f/--forks: invalid tag_value value: '{value}'"),
                true,
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::HELP;
    use crate::cli::AdhocArgs;
    use clap::CommandFactory;

    /// Options of the reference that `volant adhoc` does not take yet.
    const NOT_YET: [&str; 8] = [
        "--background",
        "--check",
        "--diff",
        "--one-line",
        "--poll",
        "--task-timeout",
        "--tree",
        "--version",
    ];

    #[test]
    fn every_option_of_the_help_is_an_argument_or_not_yet_one() {
        let mut command = AdhocArgs::command();
        command.build();
        let mut known: Vec<String> = Vec::new();
        for arg in command.get_arguments() {
            known.extend(arg.get_long().map(|l| format!("--{l}")));
            known.extend(
                arg.get_all_aliases()
                    .into_iter()
                    .flatten()
                    .map(|a| format!("--{a}")),
            );
        }
        let listed: Vec<&str> = HELP
            .lines()
            .filter(|line| line.starts_with("  -"))
            .flat_map(|line| line.trim().split(", "))
            .filter_map(|word| word.split(' ').next())
            .filter(|word| word.starts_with("--"))
            .collect();
        assert!(listed.len() > 30, "the help lists its options: {listed:?}");
        for option in listed {
            assert!(
                known.iter().any(|k| k == option) || NOT_YET.contains(&option),
                "{option} is in the help and neither an argument nor on the not-yet list"
            );
        }
    }
}
