// SPDX-License-Identifier: GPL-3.0-or-later
//! `Templar::render_file`: the one-pass render the `template` module uses. A template's own text
//! is author content read on the controller; a value it reads from a managed host is inserted as
//! text, and the result is never itself rendered, unlike `Templar::render_in`'s extra passes over
//! a task's arguments.

use std::borrow::Cow;

use minijinja::syntax::SyntaxConfig;

use super::{Templar, TemplateError, Vars, context_of, convert_error, render_str, take_failure};

mod overrides;

/// The tokens that open and close a template's tags, as Jinja2 reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Syntax {
    pub variable_start: String,
    pub variable_end: String,
    pub block_start: String,
    pub block_end: String,
    pub comment_start: String,
    pub comment_end: String,
    /// `line_statement_prefix`: only a `#jinja2:` line sets it, and a render refuses it.
    pub line_statement_prefix: Option<String>,
    /// `line_comment_prefix`: only a `#jinja2:` line sets it, and a render refuses it.
    pub line_comment_prefix: Option<String>,
}

impl Default for Syntax {
    fn default() -> Self {
        Self {
            variable_start: "{{".into(),
            variable_end: "}}".into(),
            block_start: "{%".into(),
            block_end: "%}".into(),
            comment_start: "{#".into(),
            comment_end: "#}".into(),
            line_statement_prefix: None,
            line_comment_prefix: None,
        }
    }
}

/// The longest delimiter read, in characters. The lexer builds a table from the three start
/// strings whose cost grows with the square of their length, and no real delimiter is near this.
pub const MAX_DELIMITER: usize = 64;

/// What the reference says when two of the three start strings are the same.
pub const START_CLASH: &str = "Block, variable and comment start strings must be different.";

impl Syntax {
    /// The reference's check on the three start strings, the only one it makes.
    pub fn check_starts(&self) -> Result<(), &'static str> {
        let (b, v, c) = (&self.block_start, &self.variable_start, &self.comment_start);
        if b == v || v == c || c == b {
            return Err(START_CLASH);
        }
        Ok(())
    }

    /// The bound on the length of each delimiter. The error names the option.
    pub fn check_lengths(&self) -> Result<(), String> {
        for (name, value) in [
            ("variable_start_string", &self.variable_start),
            ("variable_end_string", &self.variable_end),
            ("block_start_string", &self.block_start),
            ("block_end_string", &self.block_end),
            ("comment_start_string", &self.comment_start),
            ("comment_end_string", &self.comment_end),
        ] {
            if value.chars().nth(MAX_DELIMITER).is_some() {
                return Err(format!(
                    "{name} must not be longer than {MAX_DELIMITER} characters"
                ));
            }
        }
        Ok(())
    }

    /// Whether `text` holds one of the three start strings: the reference's test for "this may be
    /// a template". Text that holds none is written as it is.
    pub fn is_possibly_template(&self, text: &str) -> bool {
        [&self.block_start, &self.variable_start, &self.comment_start]
            .iter()
            .any(|marker| text.contains(marker.as_str()))
    }

    /// The lexer's configuration, or what makes these strings unusable.
    fn config(&self) -> Result<SyntaxConfig, String> {
        if self == &Self::default() {
            return Ok(SyntaxConfig::default());
        }
        self.check_lengths()?;
        self.check_starts().map_err(str::to_string)?;
        let named = [
            ("variable_start_string", &self.variable_start),
            ("variable_end_string", &self.variable_end),
            ("block_start_string", &self.block_start),
            ("block_end_string", &self.block_end),
            ("comment_start_string", &self.comment_start),
            ("comment_end_string", &self.comment_end),
        ];
        if let Some((name, _)) = named.iter().find(|(_, v)| v.is_empty()) {
            return Err(format!("{name} must not be an empty string"));
        }
        // minijinja skips the whitespace before it looks for the end of a tag, so an end string
        // that begins with whitespace is never found; Jinja2 finds it. Refused rather than left
        // to fail as an unexpected character.
        for (name, end) in [
            ("variable_end_string", &self.variable_end),
            ("block_end_string", &self.block_end),
        ] {
            if end.starts_with(char::is_whitespace) {
                return Err(format!("{name} must not begin with whitespace"));
            }
        }
        // minijinja and Jinja2 differ on the lines around a line statement and on a line comment:
        // a blank line after a statement is kept by one and dropped by the other, and a comment
        // takes its newline with it in one only. Measured on ansible-core 2.19.12, so refused.
        for (name, prefix) in [
            ("line_statement_prefix", &self.line_statement_prefix),
            ("line_comment_prefix", &self.line_comment_prefix),
        ] {
            if prefix.is_some() {
                return Err(format!("{name} is not supported yet"));
            }
        }
        let mut builder = SyntaxConfig::builder();
        builder
            .variable_delimiters(self.variable_start.clone(), self.variable_end.clone())
            .block_delimiters(self.block_start.clone(), self.block_end.clone())
            .comment_delimiters(self.comment_start.clone(), self.comment_end.clone());
        builder
            .build()
            .map_err(|err| format!("these delimiters cannot be used together: {err}"))
    }
}

/// The `template` module's own options, as the reference reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRender {
    /// `trim_blocks`, true unless the task says otherwise.
    pub trim_blocks: bool,
    /// `lstrip_blocks`, false unless the task says otherwise.
    pub lstrip_blocks: bool,
    /// `keep_trailing_newline`: only a `#jinja2:` line sets it.
    pub keep_trailing_newline: bool,
    /// `newline_sequence`, one of "\n", "\r", "\r\n" once the escaped spellings are read.
    pub newline_sequence: String,
    /// The delimiters, `{{ }}`, `{% %}` and `{# #}` unless the task says otherwise.
    pub syntax: Syntax,
    /// The name a render error gives the template: its `src` as the task wrote it. `None` is
    /// minijinja's own `<string>`.
    pub name: Option<String>,
}

impl Default for FileRender {
    fn default() -> Self {
        Self {
            trim_blocks: true,
            lstrip_blocks: false,
            keep_trailing_newline: false,
            newline_sequence: "\n".to_string(),
            syntax: Syntax::default(),
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
    /// Four things ansible-core's own `template` action does around Jinja2's render are
    /// reproduced here rather than left to minijinja, read from the reference's source
    /// (`plugins/action/template.py`, `_post_render_mutation` and `is_possibly_template` in
    /// `_internal/_templating/`):
    ///
    /// 0. A text with none of the three start strings is not a template: it is written as it is,
    ///    its line ends included. A first line `#jinja2:` makes it one, and overrides the
    ///    options the task gave.
    /// 1. Jinja2's lexer normalises every `\r\n` and lone `\r` in the source to `\n` before it
    ///    ever sees the template; minijinja does not, so the same normalisation happens here.
    /// 2. Jinja2 itself renders with `keep_trailing_newline=False`, which can drop more than the
    ///    reference keeps once a block tag's `trim_blocks` also eats a newline right beside it;
    ///    the reference's own fix is not to change that, but to count the trailing `\n` of the
    ///    original text and of Jinja's result afterwards, and append the difference. Doing the
    ///    same here, on the *original*, non-normalised text, is what makes `trailing_newlines`
    ///    read `text` rather than the normalised copy that was actually rendered.
    /// 3. The newline sequence of a `#jinja2:` line turns the template's own line ends, and the
    ///    appended ones are the task's `newline_sequence`, as in the reference.
    pub fn render_file<'a>(
        &self,
        text: &str,
        vars: impl Into<Vars<'a>>,
        options: &FileRender,
    ) -> Result<String, TemplateError> {
        // Before anything reads the text for a start string, so a long one costs nothing.
        options
            .syntax
            .check_lengths()
            .map_err(|msg| TemplateError(format!("Syntax error in template: {msg}")))?;
        let header = overrides::split(text, options).map_err(TemplateError)?;
        let (body, effective) = match &header {
            Some((body, effective)) => (*body, Cow::Borrowed(effective)),
            None if options.syntax.is_possibly_template(text) => (text, Cow::Borrowed(options)),
            None => return Ok(text.to_string()),
        };
        let syntax = effective
            .syntax
            .config()
            .and_then(|syntax| {
                syntax
                    .to_builder()
                    .keep_trailing_newline(effective.keep_trailing_newline)
                    .lstrip_blocks(effective.lstrip_blocks)
                    .trim_blocks(effective.trim_blocks)
                    .build()
                    .map_err(|err| format!("these delimiters cannot be used together: {err}"))
            })
            .map_err(|msg| TemplateError(format!("Syntax error in template: {msg}")))?;
        let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
        let (ctx, _tainted) = context_of(self, vars.into());
        let name = options.name.as_deref().unwrap_or("<string>");
        let rendered = render_str(self, name, &normalized, &ctx, syntax).map_err(convert_error);
        let mut rendered = match take_failure(&ctx) {
            Some(error) => return Err(error),
            None => rendered?,
        };
        if effective.newline_sequence != "\n" {
            rendered = rendered.replace('\n', &effective.newline_sequence);
        }
        let wanted = trailing_newlines(text);
        let got = trailing_newlines(&rendered);
        if wanted > got {
            rendered.push_str(&options.newline_sequence.repeat(wanted - got));
        }
        Ok(rendered)
    }
}

/// The count of literal `\n` characters at the very end of `s`, stopping at the first character
/// that is not one (a `\r` included): on `"x\r\n\r\n"` this is 1, not 2, which is what makes it
/// match the reference's own count on the text it was given, before any `\r\n` normalisation.
pub(super) fn trailing_newlines(s: &str) -> usize {
    s.chars().rev().take_while(|&c| c == '\n').count()
}

#[cfg(test)]
mod tests;
