// SPDX-License-Identifier: GPL-3.0-or-later
//! The warnings and error blocks a console session carries from one command to the next: the
//! reference's `Display` writes each one once for the whole process.

use super::Renderer;

/// What [`Renderer::take_shown`] hands over.
#[derive(Debug, Default)]
pub struct Shown {
    warned: std::collections::HashSet<String>,
    errored: std::collections::HashSet<String>,
}

impl Renderer {
    /// The warnings and error blocks written so far.
    pub fn take_shown(&mut self) -> Shown {
        Shown {
            warned: std::mem::take(&mut self.warned),
            errored: std::mem::take(&mut self.errored),
        }
    }

    /// The warnings and error blocks an earlier command wrote, which are not written again.
    pub fn set_shown(&mut self, shown: Shown) {
        self.warned = shown.warned;
        self.errored = shown.errored;
    }
}
