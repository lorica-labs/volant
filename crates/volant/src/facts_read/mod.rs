// SPDX-License-Identifier: GPL-3.0-or-later
//! Which gathered facts a run can read, found before the first connection, and whether the
//! agent's native `setup` produces every one of them.
//!
//! The native collector leaves out every key it cannot produce
//! ([`volant_protocol::facts::NATIVE_FACT_KEYS`]), and a missing key shows only once something
//! reads it. So `--facts auto` sends `setup` to the native only when every fact name the run can
//! read is in that list. The scan reads the text as written and never renders it. Whatever it
//! cannot pin to a literal name reads as every fact ([`FactsRead::All`]), which keeps the Python
//! `setup`.

use std::collections::BTreeSet;

mod decide;
mod expr;
mod lex;
mod names;
mod scan;

pub(crate) use decide::{decide, facts_read};

/// What a run can read of the facts `setup` gathers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactsRead {
    /// Any fact at all: the construct that can reach one by a computed name, and where it is.
    All(String),
    /// These keys of `ansible_facts`, without the `ansible_` prefix.
    Keys(BTreeSet<String>),
}

#[cfg(test)]
mod tests;
