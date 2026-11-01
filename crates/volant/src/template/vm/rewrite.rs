// SPDX-License-Identifier: GPL-3.0-or-later
//! The rewrite of compiled code before it runs: the operators and calls minijinja gives no hook
//! for are swapped for calls to the globals that do what the reference does.

use minijinja::machinery::{Instruction, Instructions};

use super::super::bridged;
use super::super::env::{attr_call, const_call, macro_call, var_call};
use super::super::order::OPERATORS;
use super::super::pymethods::mutable::MUTATORS;
use super::super::{
    ADD_KEY, BYTES_ADD_KEY, CONCAT_KEY, CONST_KEY, ITEM_KEY, MITEM_KEY, MKLIST_KEY, MKMAP_KEY,
    QUERY_KEY, REM_KEY, REMT_KEY, SLICE_KEY, SUB_KEY, VAULT_NAME,
};
#[cfg(doc)]
use super::concat;

/// Whether the code holds a capture that prints into the output (see [`route_calls`]), whether
/// it calls `now()`, whether it calls `encode`, which makes bytes, the names a macro or a `set`
/// takes (one called `q` or `query` shadows the lookup shorthand), and the macros' alone.
fn scan(instructions: &Instructions<'_>) -> (bool, bool, bool, Vec<String>, Vec<String>) {
    let mut captures = false;
    let mut clock = false;
    let mut bytes = false;
    // The captures open at this point, each with whether a `{{ }}` printed inside it.
    let mut open: Vec<bool> = Vec::new();
    let mut shadowed = Vec::new();
    let mut macros = Vec::new();
    let mut at = 0;
    while let Some(op) = instructions.get(at) {
        if let Instruction::BuildMacro(name, ..) | Instruction::StoreLocal(name) = op {
            shadowed.push(name.to_string());
        }
        if let Instruction::BuildMacro(name, ..) = op {
            macros.push(name.to_string());
        }
        match op {
            Instruction::CallFunction("now", _) => clock = true,
            Instruction::CallMethod("encode", _) => bytes = true,
            Instruction::BeginCapture(..) => open.push(false),
            Instruction::Emit => open.iter_mut().for_each(|printed| *printed = true),
            Instruction::EndCapture => {
                let printed = open.pop().unwrap_or(true);
                captures |= printed || !stored_after(instructions, at + 1);
            }
            _ => {}
        }
        at += 1;
    }
    (captures, clock, bytes, shadowed, macros)
}

/// Whether the captured text at `at`, after any filter of a `set` block, goes to a variable
/// rather than to the output.
fn stored_after(instructions: &Instructions<'_>, mut at: u32) -> bool {
    while let Some(op) = instructions.get(at) {
        match op {
            Instruction::StoreLocal(_) | Instruction::SetAttr(_) => return true,
            Instruction::LoadConst(_)
            | Instruction::Lookup(_)
            | Instruction::GetAttr(_)
            | Instruction::ApplyFilter(..) => at += 1,
            _ => return false,
        }
    }
    false
}

/// Replaces each `~` in compiled code by a call to [`concat()`], and each call to `query` or `q`
/// by a call to [`QUERY_KEY`]. minijinja evaluates `~` in its VM with no hook, and its syntax tree
/// cannot be rewritten in place, so the swap happens on the bytecode: the concatenation
/// instruction and a two-argument call take the same two operands off the stack in the same
/// order, and a call keeps its own, so every jump target stays where it was.
///
/// The comparison operators are routed the same way, to the globals of [`OPERATORS`]. A link of a
/// chained comparison (`a < b < c`) keeps its right operand on the stack for the next link, which
/// a call cannot do in one instruction: it jumps to code appended after the end, which calls,
/// unpacks the result and the operand, and jumps back. `in` and `not in` stay minijinja's. A
/// comparison of literals alone (`'a' < 1`) reaches this rewrite only because the source was
/// written so that minijinja's compiler cannot fold it, see [`super::literals`].
///
/// `mutating` is true when the template calls a method that changes a list or mapping in place
/// (see [`mutates`]). Each variable, attribute and item it reads then goes through a function that
/// hands back the mutable form of a plain list or mapping.
///
/// Says whether the code opens a capture (`{% filter %}`, a `{% set x %}` block that prints an
/// expression), whose `{{ }}` print into a buffer, not into the output. A `set` block whose body
/// is only text is no such capture: its text goes to the variable, and nothing it does counts
/// as a node of the output.
pub(in crate::template) fn route_calls(
    instructions: &mut Instructions<'_>,
    mutating: bool,
) -> bool {
    let (captures, clock, bytes, shadowed, macros) = scan(instructions);
    let mut links = Vec::new();
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
        let tuple_before = idx > 0 && writes_a_tuple(instructions.get(idx - 1));
        let Some(op) = instructions.get_mut(idx) else {
            break;
        };
        if let Some(named) = named {
            *op = Instruction::LoadConst(minijinja::Value::from(named));
        }
        match op {
            Instruction::StringConcat => *op = Instruction::CallFunction(CONCAT_KEY, Some(2)),
            Instruction::Eq
            | Instruction::Ne
            | Instruction::Lt
            | Instruction::Lte
            | Instruction::Gt
            | Instruction::Gte => {
                let symbol = match op {
                    Instruction::Eq => "==",
                    Instruction::Ne => "!=",
                    Instruction::Lt => "<",
                    Instruction::Lte => "<=",
                    Instruction::Gt => ">",
                    _ => ">=",
                };
                if let Some((key, _)) = operator(symbol) {
                    *op = Instruction::CallFunction(key, Some(2));
                }
            }
            // minijinja names no type for the operator a link holds, only its debug form.
            Instruction::CompareAndPreserve(cmp) => {
                if let Some((_, keep)) = link_symbol(&format!("{cmp:?}")).and_then(operator) {
                    links.push((idx, keep));
                }
            }
            // Bytes take part in `+` and in a slice only through the routed operator, which
            // hands any other operands on.
            Instruction::Add if bytes => *op = Instruction::CallFunction(BYTES_ADD_KEY, Some(2)),
            Instruction::Slice if bytes => *op = Instruction::CallFunction(SLICE_KEY, Some(4)),
            // A `datetime` takes part in `+` and `-` only through the routed operator.
            Instruction::Add if clock => *op = Instruction::CallFunction(ADD_KEY, Some(2)),
            Instruction::Sub if clock => *op = Instruction::CallFunction(SUB_KEY, Some(2)),
            Instruction::Rem => {
                let key = if tuple_before { REMT_KEY } else { REM_KEY };
                *op = Instruction::CallFunction(key, Some(2));
            }
            Instruction::GetItem => {
                let key = if mutating { MITEM_KEY } else { ITEM_KEY };
                *op = Instruction::CallFunction(key, Some(2));
            }
            Instruction::GetAttr(name) => {
                *op = Instruction::CallFunction(attr_call(name, mutating), Some(1));
            }
            Instruction::BuildList(Some(n)) if mutating && u16::try_from(*n).is_ok() => {
                *op = Instruction::CallFunction(MKLIST_KEY, u16::try_from(*n).ok());
            }
            Instruction::BuildMap(n) if mutating && u16::try_from(*n * 2).is_ok() => {
                *op = Instruction::CallFunction(MKMAP_KEY, u16::try_from(*n * 2).ok());
            }
            Instruction::LoadConst(value) if mutating => {
                if let Some(name) = const_call(value) {
                    *op = Instruction::CallFunction(name, Some(0));
                }
            }
            Instruction::Lookup(name) if mutating => {
                *op = Instruction::CallFunction(var_call(name), Some(0));
            }
            // A macro the template defines answers the value of its one node, see `macros`.
            Instruction::CallFunction(name, args) if macros.iter().any(|m| m == name) => {
                *op = Instruction::CallFunction(macro_call(name), *args);
            }
            Instruction::CallFunction(name, args)
                if matches!(*name, "query" | "q") && !shadowed.iter().any(|n| n == name) =>
            {
                *op = Instruction::CallFunction(QUERY_KEY, *args);
            }
            _ => {}
        }
        idx += 1;
    }
    if !links.is_empty() {
        // Running off the end of the code is how a render ends, `extends` included: the code
        // that runs into what is appended jumps past it instead.
        instructions.add(Instruction::Jump(u32::MAX));
        for (at, keep) in links {
            let start = append_at(instructions, at, Instruction::CallFunction(keep, Some(2)));
            append_at(instructions, at, Instruction::UnpackList(2));
            append_at(instructions, at, Instruction::Jump(at + 1));
            if let Some(op) = instructions.get_mut(at) {
                *op = Instruction::Jump(start);
            }
        }
    }
    captures
}

/// The operator a link of a chained comparison holds, from the debug form of its operator (`Lt`);
/// `None` for `in` and `not in`.
fn link_symbol(debug: &str) -> Option<&'static str> {
    Some(match debug {
        "Eq" => "==",
        "Ne" => "!=",
        "Lt" => "<",
        "Lte" => "<=",
        "Gt" => ">",
        "Gte" => ">=",
        _ => return None,
    })
}

/// The globals of a comparison operator: the one that answers, and the one a link of a chain
/// calls.
fn operator(symbol: &str) -> Option<(&'static str, &'static str)> {
    OPERATORS
        .iter()
        .find(|(op, ..)| *op == symbol)
        .map(|(_, key, keep)| (*key, *keep))
}

/// Appends `op` to the code, at the source location of the instruction at `at`, so that an error
/// it raises points where the operator was written.
fn append_at<'s>(instructions: &mut Instructions<'s>, at: u32, op: Instruction<'s>) -> u32 {
    if let Some(span) = instructions.get_span(at) {
        return instructions.add_with_span(op, span);
    }
    match instructions
        .get_line(at)
        .and_then(|line| u16::try_from(line).ok())
    {
        Some(line) => instructions.add_with_line(op, line),
        None => instructions.add(op),
    }
}

/// Whether an instruction pushes a tuple or a list written out in the template, `(a, b)` or
/// `['a', 'b']`: what stands on the right of `%` as a tuple does. A list that a variable holds, or
/// a call answers, is one argument, as it is in Python.
fn writes_a_tuple(op: Option<&Instruction<'_>>) -> bool {
    match op {
        Some(Instruction::BuildList(Some(_)) | Instruction::BuildTuple(Some(_))) => true,
        Some(Instruction::LoadConst(value)) => value.kind() == minijinja::value::ValueKind::Seq,
        // The same two, after the rewrite of a template that changes lists in place.
        Some(Instruction::CallFunction(name, _)) => {
            *name == MKLIST_KEY
                || name
                    .strip_prefix(CONST_KEY)
                    .is_some_and(|json| json.starts_with('['))
        }
        _ => false,
    }
}

/// Whether compiled code calls a method that changes a list or mapping in place.
pub(super) fn mutates(instructions: &Instructions<'_>) -> bool {
    let mut at = 0;
    while let Some(op) = instructions.get(at) {
        if let Instruction::CallMethod(name, _) = op
            && MUTATORS.contains(name)
        {
            return true;
        }
        at += 1;
    }
    false
}

#[cfg(test)]
mod tests;
