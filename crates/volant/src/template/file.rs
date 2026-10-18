// SPDX-License-Identifier: GPL-3.0-or-later
//! `Templar::render_file`: the one-pass render the `template` module uses. A template's own text
//! is author content read on the controller; a value it reads from a managed host is inserted as
//! text, and the result is never itself rendered, unlike `Templar::render_in`'s extra passes over
//! a task's arguments.

use minijinja::machinery::WhitespaceConfig;

use super::{Templar, TemplateError, Vars, context_of, convert_error, render_str};

/// The `template` module's own options, as the reference reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRender {
    /// `trim_blocks`, true unless the task says otherwise.
    pub trim_blocks: bool,
    /// `lstrip_blocks`, false unless the task says otherwise.
    pub lstrip_blocks: bool,
    /// `newline_sequence`, one of "\n", "\r", "\r\n" once the escaped spellings are read.
    pub newline_sequence: String,
    /// The name a render error gives the template: its `src` as the task wrote it. `None` is
    /// minijinja's own `<string>`.
    pub name: Option<String>,
}

impl Default for FileRender {
    fn default() -> Self {
        Self {
            trim_blocks: true,
            lstrip_blocks: false,
            newline_sequence: "\n".to_string(),
            name: None,
        }
    }
}

impl Templar {
    /// Renders a template file's text **once**, as the `template` module does: `render_str`, not
    /// `render_in`'s further passes, so a value read from a managed host lands in the output as
    /// text and is never rendered a second time. The file's own `trim_blocks`/`lstrip_blocks`
    /// are compile settings, so the `Templar`'s environment runs it unchanged.
    ///
    /// Two things ansible-core's own `template` action does after Jinja2 renders are reproduced
    /// here rather than left to minijinja, read from the reference's source
    /// (`plugins/action/template.py`, `_post_render_mutation` in
    /// `_internal/_templating/_engine.py`):
    ///
    /// 1. Jinja2's lexer normalises every `\r\n` and lone `\r` in the source to `\n` before it
    ///    ever sees the template; minijinja does not, so the same normalisation happens here.
    /// 2. Jinja2 itself renders with `keep_trailing_newline=False`, which can drop more than the
    ///    reference keeps once a block tag's `trim_blocks` also eats a newline right beside it;
    ///    the reference's own fix is not to change that, but to count the trailing `\n` of the
    ///    original text and of Jinja's result afterwards, and append the difference. Doing the
    ///    same here, on the *original*, non-normalised text, is what makes `trailing_newlines`
    ///    read `text` rather than the normalised copy that was actually rendered.
    pub fn render_file<'a>(
        &self,
        text: &str,
        vars: impl Into<Vars<'a>>,
        options: &FileRender,
    ) -> Result<String, TemplateError> {
        let ws = WhitespaceConfig {
            keep_trailing_newline: false,
            lstrip_blocks: options.lstrip_blocks,
            trim_blocks: options.trim_blocks,
        };
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let (ctx, _tainted) = context_of(vars.into());
        let name = options.name.as_deref().unwrap_or("<string>");
        let mut rendered = render_str(self, name, &normalized, &ctx, ws).map_err(convert_error)?;
        let wanted = trailing_newlines(text);
        let got = trailing_newlines(&rendered);
        if wanted > got {
            rendered.extend(std::iter::repeat_n('\n', wanted - got));
        }
        Ok(if options.newline_sequence == "\n" {
            rendered
        } else {
            rendered.replace('\n', &options.newline_sequence)
        })
    }
}

/// The count of literal `\n` characters at the very end of `s`, stopping at the first character
/// that is not one (a `\r` included): on `"x\r\n\r\n"` this is 1, not 2, which is what makes it
/// match the reference's own count on the text it was given, before any `\r\n` normalisation.
fn trailing_newlines(s: &str) -> usize {
    s.chars().rev().take_while(|&c| c == '\n').count()
}

#[cfg(test)]
mod tests;
