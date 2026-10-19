// SPDX-License-Identifier: GPL-3.0-or-later
//! The rewrite of compiled code before it runs: the operators and calls minijinja gives no hook
//! for are swapped for calls to the globals that do what the reference does.

use minijinja::machinery::{Instruction, Instructions};

use super::super::bridged;
use super::super::env::{attr_call, const_call, var_call};
use super::super::pymethods::mutable::MUTATORS;
use super::super::{
    CONCAT_KEY, CONST_KEY, ITEM_KEY, MITEM_KEY, MKLIST_KEY, MKMAP_KEY, QUERY_KEY, REM_KEY,
    REMT_KEY, VAULT_NAME,
};
#[cfg(doc)]
use super::concat;

/// Replaces each `~` in compiled code by a call to [`concat()`], and each call to `query` or `q`
/// by a call to [`QUERY_KEY`]. minijinja evaluates `~` in its VM with no hook, and its syntax tree
/// cannot be rewritten in place, so the swap happens on the bytecode: the concatenation
/// instruction and a two-argument call take the same two operands off the stack in the same
/// order, and a call keeps its own, so every jump target stays where it was.
///
/// `mutating` is true when the template calls a method that changes a list or mapping in place
/// (see [`mutates`]). Each variable, attribute and item it reads then goes through a function that
/// hands back the mutable form of a plain list or mapping.
///
/// Says whether the code opens a capture (`{% set x %}...{% endset %}`, `{% filter %}`), whose
/// `{{ }}` print into a buffer, not into the output.
pub(in crate::template) fn route_calls(
    instructions: &mut Instructions<'_>,
    mutating: bool,
) -> bool {
    let mut captures = false;
    // A macro or a `set` that takes the name `q` or `query` shadows the lookup shorthand.
    let mut shadowed = Vec::new();
    let mut at = 0;
    while let Some(op) = instructions.get(at) {
        if let Instruction::BuildMacro(name, ..) | Instruction::StoreLocal(name) = op {
            shadowed.push(name.to_string());
        }
        captures |= matches!(op, Instruction::BeginCapture(..));
        at += 1;
    }
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
            Instruction::CallFunction(name, args)
                if matches!(*name, "query" | "q") && !shadowed.iter().any(|n| n == name) =>
            {
                *op = Instruction::CallFunction(QUERY_KEY, *args);
            }
            _ => {}
        }
        idx += 1;
    }
    captures
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
