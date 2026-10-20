// SPDX-License-Identifier: GPL-3.0-or-later
//! Running compiled templates: the bytecode rewrite, one-expression detection and the
//! conversion of minijinja's errors into [`TemplateError`].

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use minijinja::machinery::{
    CodeGenerator, CompiledTemplate, Instruction, TemplateConfig, eval, make_string_output,
    parse_expr,
};
use minijinja::syntax::SyntaxConfig;
use minijinja::{AutoEscape, ErrorKind};
use serde_json::Value;

mod expression;

pub(super) use expression::single_expression;

use super::bridged;
use super::context::set_literal;
use super::env::{container_repr, holds_undefined};
use super::pymethods::format::{holds_tuple, repr, to_str};
use super::templar::Templar;
use super::{TemplateError, UNDEFINED};

mod rewrite;

use rewrite::mutates;
pub(super) use rewrite::route_calls;

/// Replaces every `!unsafe` wrapper inside `value` by its text, and every `!vault` wrapper that
/// decrypts by what it holds; whether there was one. A vault wrapper that does not decrypt stays
/// as it is, and raises when something renders it.
pub(super) fn unwrap_unsafe(value: &mut Value) -> bool {
    match value {
        Value::String(s) => {
            let text = crate::yaml::unsafe_text(s)
                .or_else(|| crate::yaml::vault_text(s).and_then(|c| crate::vault::reveal(&c).ok()));
            text.map(|t| *s = t).is_some()
        }
        Value::Array(items) => items.iter_mut().fold(false, |a, v| unwrap_unsafe(v) | a),
        Value::Object(map) => map.values_mut().fold(false, |a, v| unwrap_unsafe(v) | a),
        _ => false,
    }
}

/// Whether any string inside a value still carries a template marker.
pub(crate) fn holds_template(value: &Value) -> bool {
    match value {
        Value::String(s) => Templar::is_stored_template(s),
        Value::Array(items) => items.iter().any(holds_template),
        Value::Object(map) => map.values().any(holds_template),
        _ => false,
    }
}

/// What a template reads: the names it looks up, and whether it calls something that looks a
/// name up by text (`lookup`, `query`, `q`).
#[derive(Debug, Default)]
pub(super) struct TemplateReads {
    pub(super) names: BTreeSet<String>,
    pub(super) dynamic: bool,
}

/// The names `source` looks up, read off the compiled code. `None` when it does not compile.
/// Names a `set` or a loop binds inside the template are listed too, which only makes the
/// answer more careful.
pub(super) fn template_reads(source: &str) -> Option<TemplateReads> {
    let config = TemplateConfig {
        syntax_config: SyntaxConfig::default(),
        default_auto_escape: Arc::new(|_| AutoEscape::None),
    };
    let compiled = no_panic(|| CompiledTemplate::new("<reads>", source, &config)).ok()?;
    let mut reads = TemplateReads::default();
    for instructions in std::iter::once(&compiled.instructions).chain(compiled.blocks.values()) {
        let mut idx = 0;
        while let Some(op) = instructions.get(idx) {
            match op {
                // The function read as a value, then called under another name, is a lookup by
                // text just the same.
                Instruction::Lookup(name) if matches!(*name, "lookup" | "query" | "q") => {
                    reads.dynamic = true;
                }
                Instruction::Lookup(name) => {
                    reads.names.insert((*name).to_string());
                }
                Instruction::CallFunction(name, _) if matches!(*name, "lookup" | "query" | "q") => {
                    reads.dynamic = true;
                }
                _ => {}
            }
            idx += 1;
        }
    }
    Some(reads)
}

/// `a ~ b`. minijinja's own `~` only refuses an operand that is undefined itself and prints a
/// list or mapping holding one as `[1, undefined]`; measured on ansible-core 2.19.12,
/// `'x' ~ [1, nope]` and `'x' ~ {'k': nope}` are undefined reads, like `'x' ~ nope`.
pub(super) fn concat(
    left: minijinja::Value,
    right: minijinja::Value,
) -> Result<minijinja::Value, minijinja::Error> {
    if holds_undefined(&left) || holds_undefined(&right) {
        return Err(minijinja::Error::from(ErrorKind::UndefinedError));
    }
    let shown = |v: &minijinja::Value| {
        if holds_tuple(v) {
            return repr(v);
        }
        // Python's `str` of a float: `nan`, `1e+16`.
        if v.kind() == minijinja::value::ValueKind::Number && !v.is_integer() {
            return to_str(v);
        }
        container_repr(v).unwrap_or_else(|| v.to_string())
    };
    Ok(minijinja::Value::from(format!(
        "{}{}",
        shown(&left),
        shown(&right)
    )))
}

/// What a render wrote through `{{ }}`, which is all a string render needs to know to tell whether
/// the template made one output node: the number of expressions printed and the first one, as a
/// value. Text between the tags is the other kind of node, and the text of the whole render
/// tells those apart (see [`Emitted::single`]).
#[derive(Default)]
pub(super) struct Emitted {
    count: u32,
    first: Option<minijinja::Value>,
    /// Where the render's own output lives. A macro prints into an output of its own.
    top: usize,
    /// The code holds a capture, which prints into the same output through a buffer: the count
    /// is not the number of nodes then.
    captures: bool,
    /// An expression printed `none`, which is no node and so can leave one node of the rest.
    none: bool,
}

/// What a template that makes no more than one node is worth, once it has rendered to `text`.
pub(super) enum Node {
    /// No node at all: the reference's result is `None`.
    None,
    /// Exactly one expression and no text: the reference keeps its type.
    Value(minijinja::Value),
    /// Text, or several nodes: a string.
    Text,
}

impl Emitted {
    /// Whether an expression printed `none`.
    pub(super) fn printed_none(&self) -> bool {
        self.none
    }

    /// The reference joins the nodes of a template into one string, except when there is one node
    /// (its value, as it is) or none (`None`). `text` is what the whole render wrote. An
    /// expression that is the only thing written has written exactly its own text; any more text
    /// means a text node beside it. A capture prints through a buffer into the same output, so
    /// with one in the code the count says nothing: the result is a string, or `None` when nothing
    /// was printed at all.
    pub(super) fn single(self, text: &str) -> Node {
        if self.captures {
            return if text.is_empty() && self.count == 0 {
                Node::None
            } else {
                Node::Text
            };
        }
        match (self.count, self.first) {
            (0, _) if text.is_empty() => Node::None,
            (1, Some(value)) => {
                let shown = container_repr(&value).unwrap_or_else(|| value.to_string());
                if shown == text {
                    Node::Value(value)
                } else {
                    Node::Text
                }
            }
            _ => Node::Text,
        }
    }
}

/// Where `out` lives, to tell the render's own output from the one of a macro it calls.
fn address(out: &minijinja::Output<'_>) -> usize {
    std::ptr::from_ref(out) as usize
}

thread_local! {
    /// The expressions the render running on this thread has printed. A render inside a render, a
    /// lookup that renders a file, saves the outer one's and puts it back: see [`render_counted`].
    static EMITTED: RefCell<Emitted> = RefCell::new(Emitted::default());
}

/// Notes one `{{ }}` the VM is about to print into `out`. Called by the formatter of the
/// environment. A `none` is no node: the reference leaves it out of the result.
pub(super) fn note_emit(out: &minijinja::Output<'_>, value: &minijinja::Value) {
    EMITTED.with_borrow_mut(|e| {
        if address(out) != e.top {
            return;
        }
        if value.is_none() {
            e.none = true;
            return;
        }
        if e.count == 0 {
            e.first = Some(value.clone());
        }
        e.count = e.count.saturating_add(1);
    });
}

/// `env.render_named_str`, with `~` routed through [`concat()`]. Compiled once per call, as
/// `render_named_str` does, with the syntax settings given (whitespace included). The only place that
/// decides auto-escape. The environment is the templar's, with the filters and tests the
/// compiled code calls that only the Python helper has ([`Templar::env_for`]).
pub(super) fn render_str(
    templar: &Templar,
    name: &str,
    source: &str,
    ctx: &minijinja::Value,
    syntax_config: SyntaxConfig,
) -> Result<String, minijinja::Error> {
    render_counted(templar, name, source, ctx, syntax_config).map(|(text, _)| text)
}

/// [`render_str`], and what the render printed through `{{ }}`.
pub(super) fn render_counted(
    templar: &Templar,
    name: &str,
    source: &str,
    ctx: &minijinja::Value,
    syntax_config: SyntaxConfig,
) -> Result<(String, Emitted), minijinja::Error> {
    let config = TemplateConfig {
        syntax_config,
        // Never by file name: measured on ansible-core 2.19.12, `index.html.j2` holding
        // `{{ "<b>" }}` writes `<b>`.
        default_auto_escape: Arc::new(|_| AutoEscape::None),
    };
    let mut compiled = no_panic(|| CompiledTemplate::new(name, source, &config))?;
    let mutating = mutates(&compiled.instructions) || compiled.blocks.values().any(mutates);
    let mut captures = route_calls(&mut compiled.instructions, mutating);
    for block in compiled.blocks.values_mut() {
        captures |= route_calls(block, mutating);
    }
    let env =
        templar.env_for(std::iter::once(&compiled.instructions).chain(compiled.blocks.values()));
    let mut out = String::with_capacity(compiled.buffer_size_hint);
    let mut output = make_string_output(&mut out);
    let outer = EMITTED.replace(Emitted {
        top: address(&output),
        captures,
        ..Emitted::default()
    });
    let run = eval(
        &env,
        &compiled.instructions,
        ctx.clone(),
        &compiled.blocks,
        &mut output,
        compiled.initial_auto_escape,
    );
    let emitted = EMITTED.replace(outer);
    run?;
    Ok((out, emitted))
}

/// What a render says when the lexer of minijinja failed on a line it cannot report on.
pub(super) const LONG_LINE: &str =
    "the template has a syntax error on a line too long to be reported";

/// Runs the lexer and the parser of minijinja, and turns a panic of theirs into an error. Its
/// lexer counts the columns of a line in sixteen bits, and a syntax error 65,535 characters into
/// a line overflows that counter, which panics in a debug build. Text from a playbook must not
/// be able to stop the run.
fn no_panic<T>(
    compile: impl FnOnce() -> Result<T, minijinja::Error>,
) -> Result<T, minijinja::Error> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(compile))
        .unwrap_or_else(|_| Err(minijinja::Error::new(ErrorKind::SyntaxError, LONG_LINE)))
}

/// `env.compile_expression(expr)?.eval(ctx)`, with `~` routed through [`concat()`].
pub(super) fn eval_expr(
    templar: &Templar,
    expr: &str,
    ctx: &minijinja::Value,
) -> Result<minijinja::Value, minijinja::Error> {
    let ast = no_panic(|| parse_expr(expr))?;
    let mut codegen = CodeGenerator::new("<expression>", expr);
    codegen.compile_expr(&ast);
    let (mut instructions, _) = codegen.finish();
    let mutating = mutates(&instructions);
    let _ = route_calls(&mut instructions, mutating);
    let env = templar.env_for([&instructions]);
    let mut out = String::new();
    let (value, _) = eval(
        &env,
        &instructions,
        ctx.clone(),
        &BTreeMap::new(),
        &mut make_string_output(&mut out),
        AutoEscape::None,
    )?;
    Ok(value.unwrap_or_default())
}

pub(super) fn convert_error(err: minijinja::Error) -> TemplateError {
    // An operator given an undefined operand is an undefined read (`nope | int + 1` fails
    // `'nope' is undefined`, measured on ansible-core 2.19.12), worded as any other; minijinja
    // names the operand's kind in its message and has no other way to tell it.
    let undefined_operand = err.kind() == ErrorKind::InvalidOperation
        && err.detail().is_some_and(|d| {
            d.starts_with("tried to use ")
                && (d.contains(" unsupported types undefined and ")
                    || d.ends_with(" and undefined"))
        });
    if undefined_operand {
        return convert_error(minijinja::Error::from(ErrorKind::UndefinedError));
    }
    // A bridged call's failure is worded in full where it is raised, as the reference words it.
    if let Some(said) = bridged::said(&err) {
        return TemplateError(said.to_string());
    }
    let text = err.to_string() + super::bigint::refusal_note(&err);
    if err.kind() == ErrorKind::UndefinedError {
        return TemplateError(format!("{UNDEFINED} The error was: {text}"));
    }
    TemplateError(text)
}

/// Notes that a pass left text that must stay as it is, when the result still holds a template
/// marker and the marker is the text's own:
///
/// - no read of the pass handed out a template that was not rendered, so no stored template can
///   be in the result: the text wrote the marker itself, with a `raw` block or in a string such as
///   `'{{'`, or built it from names and text that hold none (`'{{ ' ~ name ~ ' }}'`, `replace`);
/// - or `source` has a `raw` block. `source` is the author's text on the `first` pass. On a
///   later pass it is a variable's value, and a result that still has a `raw` block is a chain
///   not read to its end.
///
/// The reference renders a result once, so such text is literal. Neither a later pass nor a later
/// read of the variable that holds it renders it again: see `render_value_tainted`.
pub(super) fn keep_literal(
    source: &str,
    result: &Value,
    ctx: &minijinja::Value,
    first: bool,
    none_deferred: bool,
) {
    if !holds_template(result) {
        return;
    }
    let raw =
        opens_raw(source) && (first || !matches!(result, Value::String(text) if opens_raw(text)));
    if none_deferred || raw {
        set_literal(ctx, true);
    }
}

/// Whether `text` holds a `{% raw %}` tag, with or without whitespace control.
fn opens_raw(text: &str) -> bool {
    let mut rest = text;
    while let Some(at) = rest.find("{%") {
        rest = &rest[at + 2..];
        let tag = rest.trim_start_matches(['-', '+']).trim_start();
        if let Some(after) = tag.strip_prefix("raw")
            && after.starts_with(|c: char| c.is_whitespace() || matches!(c, '-' | '+' | '%'))
        {
            return true;
        }
    }
    false
}
