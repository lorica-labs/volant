// SPDX-License-Identifier: GPL-3.0-or-later
//! The Python interpreters the agent looks for on a managed host, in the reference's order.
//!
//! Here rather than in the agent because the controller's own tests read it too: the check that
//! the managed host has no ansible-core has to probe every name the agent might resolve, and a
//! copy of this list kept by hand would narrow that check in silence the day the two diverge.

/// ansible-core 2.19.12's own `INTERPRETER_PYTHON_FALLBACK`, read off the reference with
/// `ansible-config dump | grep INTERPRETER` on 2026-09-20.
///
/// The order is a preference, not a set: the controller runs a module under the first entry the
/// agent reports, so rearranging this list changes which Python a playbook runs under on every
/// host that has more than one.
pub const CANDIDATES: [&str; 8] = [
    "python3.13",
    "python3.12",
    "python3.11",
    "python3.10",
    "python3.9",
    "python3.8",
    "/usr/bin/python3",
    "python3",
];
