// SPDX-License-Identifier: GPL-3.0-or-later
//! The stdout callback a run prints through: ansible-core 2.19.12's `default`, `minimal` and
//! `oneline`, and `tree`, an aggregate plugin that prints nothing when it is named as the stdout
//! one.

use super::Renderer;

/// Where a loop's aggregate keeps its items: the `default` callback never shows them, and
/// `minimal` and `oneline` show them as the result's `results`.
pub const LOOP_RESULTS: &str = "_volant_loop_results";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StdoutCallback {
    Default,
    Minimal,
    Oneline,
    Tree,
    /// ansible.posix's `json`, which `json` routes to.
    Json,
}

impl StdoutCallback {
    /// The callback a name loads, short or in full; `None` for one this engine does not have.
    pub fn named(name: &str) -> Option<Self> {
        match name.strip_prefix("ansible.builtin.").unwrap_or(name) {
            "default" => Some(Self::Default),
            "minimal" => Some(Self::Minimal),
            "oneline" => Some(Self::Oneline),
            "tree" => Some(Self::Tree),
            "json" | "ansible.posix.json" => Some(Self::Json),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Minimal => "minimal",
            Self::Oneline => "oneline",
            Self::Tree => "tree",
            Self::Json => "ansible.posix.json",
        }
    }

    /// The release that removes it, for the two ansible-core 2.19.12 deprecates.
    pub fn removed_in(self) -> Option<&'static str> {
        matches!(self, Self::Oneline | Self::Tree).then_some("2.23")
    }
}

impl Renderer {
    /// Prints through `callback` from here on. `minimal` and `oneline` take each result's command
    /// form from the action [`Renderer::set_action`] names; `tree` prints nothing.
    pub fn set_stdout_callback(&mut self, callback: StdoutCallback) {
        if callback == StdoutCallback::Default {
            return;
        }
        self.minimal.get_or_insert_with(String::new);
        self.oneline = callback == StdoutCallback::Oneline;
        if callback == StdoutCallback::Tree {
            self.silence();
        }
        // Nothing but the document reaches stdout.
        if callback == StdoutCallback::Json {
            let out = std::mem::replace(&mut self.out, Box::new(std::io::sink()));
            self.json = Some(Box::new(super::json_doc::JsonDoc::new(out, Some(4))));
        }
    }

    /// `json_indent` of the `json` callback: spaces per level, the document on one line at zero
    /// or below.
    pub fn set_json_indent(&mut self, indent: i64) {
        if let Some(doc) = &mut self.json {
            doc.set_indent(usize::try_from(indent).ok().filter(|n| *n > 0));
        }
    }

    /// Whether a result the `default` callback keeps quiet still gets a line: a loop's
    /// aggregate, which `minimal` and `oneline` print after nothing for the items.
    pub fn shows_every_result(&self) -> bool {
        self.minimal.is_some()
    }

    /// The action of the task whose results follow, which `minimal` and `oneline` read.
    pub fn set_action(&mut self, action: &str) {
        if let Some(current) = &mut self.minimal {
            action.clone_into(current);
        }
    }
}

#[cfg(test)]
mod tests;
