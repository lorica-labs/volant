// SPDX-License-Identifier: GPL-3.0-or-later
//! Running compiled templates: the bytecode rewrite, one-expression detection and the
//! conversion of minijinja's errors into [`TemplateError`].

use std::collections::BTreeMap;
use std::sync::Arc;

use minijinja::machinery::{
    CodeGenerator, CompiledTemplate, Instruction, Instructions, TemplateConfig, Vm,
    WhitespaceConfig, make_string_output, parse_expr,
};
use minijinja::syntax::SyntaxConfig;
use minijinja::{AutoEscape, ErrorKind};
use serde_json::Value;

use super::bridged;
use super::env::{attr_call, container_repr, holds_undefined};
use super::templar::Templar;
use super::{CONCAT_KEY, ITEM_KEY, QUERY_KEY, TemplateError, UNDEFINED, VAULT_NAME};

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
pub(super) fn holds_template(value: &Value) -> bool {
    match value {
        Value::String(s) => Templar::is_template(s),
        Value::Array(items) => items.iter().any(holds_template),
        Value::Object(map) => map.values().any(holds_template),
        _ => false,
    }
}

/// `{{ expr }}` and nothing else: no text around, no second expression, no statement. A `}}`
/// inside is no end when it closes a bracket the expression opened or sits in a string literal:
/// `{'a': {'x': 5}}` closing on `}}}` and `'}}'` are one expression, `1 }}{{ 2` is two.
pub(super) fn single_expression(text: &str) -> Option<&str> {
    let t = text.trim();
    let inner = t.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{%") || inner.contains("{#") || !closes_only_what_it_opens(inner) {
        return None;
    }
    Some(inner.trim())
}

/// Whether every bracket `expr` closes outside a string literal is one it opened. minijinja's
/// own parser cannot be asked: its lexer panics on a `}` with nothing open.
fn closes_only_what_it_opens(expr: &str) -> bool {
    let mut depth = 0usize;
    let mut quote = None;
    let mut chars = expr.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(_), '\\') => {
                chars.next();
            }
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '{' | '[' | '(') => depth += 1,
            (None, '}' | ']' | ')') => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            _ => {}
        }
    }
    true
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
    let shown = |v: &minijinja::Value| container_repr(v).unwrap_or_else(|| v.to_string());
    Ok(minijinja::Value::from(format!(
        "{}{}",
        shown(&left),
        shown(&right)
    )))
}

/// Replaces each `~` in compiled code by a call to [`concat()`], and each call to `query` or `q`
/// by a call to [`QUERY_KEY`]. minijinja evaluates `~` in its VM with no hook, and its syntax tree
/// cannot be rewritten in place, so the swap happens on the bytecode: the concatenation
/// instruction and a two-argument call take the same two operands off the stack in the same
/// order, and a call keeps its own, so every jump target stays where it was.
pub(super) fn route_calls(instructions: &mut Instructions<'_>) {
    let mut idx = 0;
    loop {
        let named = match (instructions.get(idx), instructions.get(idx + 1)) {
            (Some(Instruction::Lookup(name)), Some(Instruction::PerformTest(test, ..)))
                if bridged::short(test) == "vault_encrypted" =>
            {
                Some(format!("{VAULT_NAME}{name}"))
            }
            _ => None,
        };
        let Some(op) = instructions.get_mut(idx) else {
            break;
        };
        if let Some(named) = named {
            *op = Instruction::LoadConst(minijinja::Value::from(named));
        }
        match op {
            Instruction::StringConcat => *op = Instruction::CallFunction(CONCAT_KEY, Some(2)),
            Instruction::GetItem => *op = Instruction::CallFunction(ITEM_KEY, Some(2)),
            Instruction::GetAttr(name) => {
                *op = Instruction::CallFunction(attr_call(name), Some(1));
            }
            Instruction::CallFunction(name, args) if matches!(*name, "query" | "q") => {
                *op = Instruction::CallFunction(QUERY_KEY, *args);
            }
            _ => {}
        }
        idx += 1;
    }
}

/// `env.render_named_str`, with `~` routed through [`concat()`]. Compiled once per call, as
/// `render_named_str` does, with the whitespace settings given. The only place that decides
/// auto-escape and syntax. The environment is the templar's, with the filters and tests the
/// compiled code calls that only the Python helper has ([`Templar::env_for`]).
pub(super) fn render_str(
    templar: &Templar,
    name: &str,
    source: &str,
    ctx: &minijinja::Value,
    ws_config: WhitespaceConfig,
) -> Result<String, minijinja::Error> {
    let config = TemplateConfig {
        syntax_config: SyntaxConfig,
        ws_config,
        // Never by file name: measured on ansible-core 2.19.12, `index.html.j2` holding
        // `{{ "<b>" }}` writes `<b>`.
        default_auto_escape: Arc::new(|_| AutoEscape::None),
    };
    let mut compiled = CompiledTemplate::new(name, source, &config)?;
    route_calls(&mut compiled.instructions);
    compiled.blocks.values_mut().for_each(route_calls);
    let env =
        templar.env_for(std::iter::once(&compiled.instructions).chain(compiled.blocks.values()));
    let mut out = String::with_capacity(compiled.buffer_size_hint);
    Vm::new(&env).eval(
        &compiled.instructions,
        ctx.clone(),
        &compiled.blocks,
        &mut make_string_output(&mut out),
        compiled.initial_auto_escape,
    )?;
    Ok(out)
}

/// `env.compile_expression(expr)?.eval(ctx)`, with `~` routed through [`concat()`].
pub(super) fn eval_expr(
    templar: &Templar,
    expr: &str,
    ctx: &minijinja::Value,
) -> Result<minijinja::Value, minijinja::Error> {
    let ast = parse_expr(expr)?;
    let mut codegen = CodeGenerator::new("<expression>", expr);
    codegen.compile_expr(&ast);
    let (mut instructions, _) = codegen.finish();
    route_calls(&mut instructions);
    let env = templar.env_for([&instructions]);
    let mut out = String::new();
    let (value, _) = Vm::new(&env).eval(
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
    let text = err.to_string();
    if err.kind() == ErrorKind::UndefinedError {
        return TemplateError(format!("{UNDEFINED} The error was: {text}"));
    }
    TemplateError(text)
}
