// SPDX-License-Identifier: GPL-3.0-or-later
//! The colours of the output, as ansible-core writes them.

use std::fmt;

/// A colour as ansible-core's `COLOR_CODES` writes it, `0;32` for `green`. Shown, it opens the
/// colour; shown with `#`, it closes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Style(&'static str);

impl fmt::Display for Style {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            f.write_str("\x1b[0m")
        } else {
            write!(f, "\x1b[{}m", self.0)
        }
    }
}

/// ansible-core's default `[colors]`: `ok`, `changed`, `error`, `unreachable`, `skip`, `warn`,
/// `debug` and `verbose`.
pub(super) const OK: Style = Style("0;32");
pub(super) const CHANGED: Style = Style("0;33");
pub(super) const FAILED: Style = Style("0;31");
pub(super) const UNREACHABLE: Style = Style("1;31");
pub(super) const SKIPPED: Style = Style("0;36");
pub(super) const WARNING: Style = Style("1;35");
pub(super) const DEBUG: Style = Style("1;30");
pub(crate) const VERBOSE: Style = Style("0;34");
