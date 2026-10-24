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
