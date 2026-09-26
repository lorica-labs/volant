// SPDX-License-Identifier: GPL-3.0-or-later
//! Facts shared by the controller and the agent's native collector.

/// Every fact key the agent's native `setup` produces. The controller sends `setup` to the native
/// collector only when every key the play reads is in this list: a key the collector cannot
/// produce is absent from its answer, never wrong, and a missing key goes unnoticed until
/// something reads it. Empty until the collector exists.
pub const NATIVE_FACT_KEYS: &[&str] = &[];
