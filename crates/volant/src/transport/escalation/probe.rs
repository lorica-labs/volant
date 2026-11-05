// SPDX-License-Identifier: GPL-3.0-or-later
//! What a probe of an escalation says, before a remote link is opened over it.

use super::super::bootstrap::first_words;
use super::super::defaults::{ConnectError, INCORRECT_SUDO_PASSWORD, MISSING_SUDO_PASSWORD};

/// Why `sudo` did not escalate, given that it demonstrably did not run the command it was
/// handed. `ran` is that proof: one of the bootstrap's private exit codes, which nothing but the
/// bootstrap itself can produce, so a `sudo` that exited zero having run something else is not an
/// escalation that worked.
///
/// The strings are the two `sudo` implementations' own, measured on the development machine
/// (`sudo-rs` 0.2.13) and on the target machine (`sudo` 1.9.15p5); a host may run either, so
/// both wordings are matched. Which of the two messages a match produces is decided by whether
/// `offered` carries the password this invocation wrote, rather than by the text, whose split
/// between "none given" and "wrong one" differs between the two implementations.
pub(in crate::transport) fn escalation_outcome(
    ran: bool,
    stderr: &str,
    offered: Option<&str>,
) -> Result<(), ConnectError> {
    if ran {
        return Ok(());
    }
    if refused_authentication(stderr) {
        return Err(ConnectError::Become(
            if offered.is_some() {
                INCORRECT_SUDO_PASSWORD
            } else {
                MISSING_SUDO_PASSWORD
            }
            .to_string(),
        ));
    }
    Err(ConnectError::Become(first_words(
        &redacted_stderr(stderr, offered),
        "sudo refused without a message",
    )))
}

/// Whether `sudo` refused because it wanted an authentication it did not get.
pub(super) fn refused_authentication(stderr: &str) -> bool {
    const AUTHENTICATION: &[&str] = &[
        "a password is required",
        "interactive authentication is required",
        "no password was provided",
        "Sorry, try again",
        "incorrect password",
        "Authentication failed",
        "Authentication required but not attempted",
    ];
    AUTHENTICATION.iter().any(|m| stderr.contains(m))
}

/// `sudo`'s own words on their way into a message an operator reads, with the password this
/// invocation offered it taken back out.
///
/// No `sudo` measured for this echoes the password it was given, and this text only ever
/// reaches a task result when `sudo` said something the two known implementations do not say.
/// The password is the controller's own, so the path is closed by construction rather than by
/// trusting every `sudo` on every host to keep it out of its stderr.
pub(super) fn redacted_stderr(stderr: &str, offered: Option<&str>) -> String {
    match offered {
        Some(password) if !password.is_empty() && stderr.contains(password) => {
            stderr.replace(password, "<redacted>")
        }
        _ => stderr.to_string(),
    }
}

/// `run_capturing`'s message for an `ssh` that exited 255, with the same redaction
/// `escalation_outcome` gives `sudo`'s own refusal. `ssh`'s own words are worth keeping here -
/// they are how an operator tells a dead host from a refused key from a closed port - so this
/// scrubs the password out rather than dropping `stderr` outright.
pub(in crate::transport) fn unreachable_message(stderr: &str, offered: Option<&str>) -> String {
    // The `Task failed: ` prefix is the reference's own, measured: it opens the `msg` of an
    // UNREACHABLE and of a task that dies on a conditional, so a playbook testing
    // `'Task failed' in result.msg` has to find it here too.
    format!(
        "Task failed: Failed to connect to the host via ssh: {}",
        first_words(
            &redacted_stderr(stderr, offered),
            "ssh failed without a message"
        )
    )
}
