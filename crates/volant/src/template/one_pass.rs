// SPDX-License-Identifier: GPL-3.0-or-later
//! One render with no second pass, and whether it read data.

use std::sync::atomic::Ordering;

use serde_json::Value;

use super::{Templar, TemplateError, Vars, context_of};

impl Templar {
    /// Renders `text` once. What the pass makes is the result, whatever it holds.
    pub(crate) fn render_one_pass<'a>(
        &self,
        text: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<Value, TemplateError> {
        Ok(self.render_one_pass_tainted(text, vars)?.0)
    }

    /// `render_one_pass`, saying as well whether the pass read a value from a managed host.
    pub(crate) fn render_one_pass_tainted<'a>(
        &self,
        text: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<(Value, bool), TemplateError> {
        let (ctx, tainted) = context_of(self, vars.into());
        let value = self.render_once(text, &ctx)?;
        Ok((value, tainted.load(Ordering::Relaxed)))
    }
}
