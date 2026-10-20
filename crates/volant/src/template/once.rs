// SPDX-License-Identifier: GPL-3.0-or-later
//! One pass over a string, as the reference's `Templar.template` does it.

use std::borrow::Cow;

use minijinja::syntax::SyntaxConfig;
use serde_json::Value;

use super::file::trailing_newlines;
use super::vm::{LONG_LINE, Node, convert_error, render_counted, single_expression};
use super::{Templar, TemplateError, take_failure};

impl Templar {
    /// Whether the reference would render `text`: it holds `{{`, `{%` or `{#`. A substring test,
    /// because every string a render reads goes through it.
    pub fn is_template(text: &str) -> bool {
        let mut rest = text;
        while let Some(at) = rest.find('{') {
            if matches!(rest.as_bytes().get(at + 1), Some(b'{' | b'%' | b'#')) {
                return true;
            }
            rest = &rest[at + 1..];
        }
        false
    }

    /// Whether a string that is already stored, as a variable or a fact, is to be rendered when it
    /// is read: it holds `{{` or `{%`, or a comment that closes. A `{#` with no `#}` after it is
    /// left alone, because text an expression made (`{{ 'x{#' }}`) looks like that and is data:
    /// the reference does not render it again.
    pub(crate) fn is_stored_template(text: &str) -> bool {
        let mut rest = text;
        while let Some(at) = rest.find('{') {
            match rest.as_bytes().get(at + 1) {
                Some(b'{' | b'%') => return true,
                Some(b'#') if rest[at + 2..].contains("#}") => return true,
                _ => {}
            }
            rest = &rest[at + 1..];
        }
        false
    }

    /// Whether `text` holds `{{` or `{%`: a rendered result that does is rendered again. `{#` is
    /// left out on purpose: a result that holds one is text an expression made (`{{ '{#' }}`),
    /// and the reference does not render it again.
    pub(super) fn holds_expression(text: &str) -> bool {
        text.contains("{{") || text.contains("{%")
    }

    /// Whether `text` holds a statement block or a comment, the two tags that print nothing and so
    /// can leave a result with one node (or none) where the text around them is not a node.
    fn has_statement(text: &str) -> bool {
        text.contains("{%") || text.contains("{#")
    }

    /// One pass over a string, as the reference's `Templar.template` does it: Jinja2 with
    /// `trim_blocks` on and `lstrip_blocks` off, one trailing line end dropped before the lexer
    /// runs and put back on a string result, and the output nodes joined into a string unless
    /// there is one node (its value, with its type) or none (`None`).
    pub(super) fn render_once(
        &self,
        text: &str,
        ctx: &minijinja::Value,
    ) -> Result<Value, TemplateError> {
        if !Self::is_template(text) {
            return Ok(Value::String(text.to_string()));
        }
        // The lexer reads a lone `\r` and `\r\n` as `\n`, then drops one line end at the end.
        let source = if text.contains('\r') {
            Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
        } else {
            Cow::Borrowed(text)
        };
        let body = source.strip_suffix('\n').unwrap_or(&source);
        match self.render_body(text, body, ctx) {
            // The lexer of the reference stops quietly when a comment opens at the very end of
            // the source. A `{#` only counts for that when it opens one, which is why the render
            // is tried first: `{{#` is a syntax error.
            Err(err)
                if err.0.contains("unexpected end of comment") || err.0.contains(LONG_LINE) =>
            {
                let cut = without_open_comment(body);
                if cut.len() == body.len() {
                    Err(err)
                } else {
                    self.render_body(text, cut, ctx)
                }
            }
            other => other,
        }
    }

    /// [`Templar::render_once`] on `body`, the text with its one line end dropped.
    fn render_body(
        &self,
        text: &str,
        body: &str,
        ctx: &minijinja::Value,
    ) -> Result<Value, TemplateError> {
        if let Some(expr) = lone_expression(body) {
            let value = self.evaluate_in(expr, ctx)?;
            return Ok(with_line_ends(value, text));
        }
        // The line end is dropped by hand, so the lexer is told to keep what is left.
        let syntax = SyntaxConfig::builder()
            .keep_trailing_newline(true)
            .trim_blocks(true)
            .build()
            .map_err(|err| TemplateError(err.to_string()))?;
        let counted = render_counted(self, "<string>", body, ctx, syntax).map_err(convert_error);
        // A variable the text read failed to render: that is the error, whatever the undefined
        // value it was read as did to the text.
        if let Some(error) = take_failure(ctx) {
            return Err(error);
        }
        let (out, emitted) = counted?;
        // Without a statement, a comment or a `none` every `{{ }}` is a node of its own and the
        // text between is one too, so one node means the text is one expression, handled above.
        if Self::has_statement(text) || emitted.printed_none() {
            match emitted.single(&out) {
                Node::None => return Ok(Value::Null),
                Node::Value(value) => {
                    let value = super::pymethods::bytes::to_json(&value)
                        .map_err(|e| TemplateError(super::pymethods::bytes::result_error(&e)))?;
                    return Ok(with_line_ends(value, text));
                }
                Node::Text => {}
            }
        }
        Ok(with_line_ends(Value::String(out), text))
    }
}

/// `body` without a comment that opens at its very end and never closes. The reference accepts
/// one, and it means nothing. `{#-` also strips the text before it. Anything after the `{#`, a
/// space included, makes it an error, so the text is left as it is then.
fn without_open_comment(body: &str) -> &str {
    if let Some(rest) = body.strip_suffix("{#") {
        rest
    } else if let Some(rest) = body.strip_suffix("{#-") {
        rest.trim_end()
    } else if let Some(rest) = body.strip_suffix("{#+") {
        rest
    } else {
        body
    }
}

/// The expression of a string that is one `{{ }}` and no text. Whitespace around it is text, and
/// so makes a string of the result, unless the tag strips it: `{{-` takes the whitespace before
/// it and `-}}` the whitespace after it.
fn lone_expression(body: &str) -> Option<&str> {
    let tag = body.trim();
    let expr = single_expression(tag)?;
    let before = body.len() - body.trim_start().len();
    let after = body.len() - body.trim_end().len();
    if (before > 0 && !tag.starts_with("{{-")) || (after > 0 && !tag.ends_with("-}}")) {
        return None;
    }
    Some(expr)
}

/// A string result gets back the line ends the lexer dropped from the end of `template`: as many
/// as the template had beyond the ones the result has, as the reference counts them.
fn with_line_ends(mut value: Value, template: &str) -> Value {
    if let Value::String(text) = &mut value {
        let wanted = trailing_newlines(template);
        let got = trailing_newlines(text);
        if wanted > got {
            text.extend(std::iter::repeat_n('\n', wanted - got));
        }
    }
    value
}
