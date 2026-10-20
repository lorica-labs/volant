// SPDX-License-Identifier: GPL-3.0-or-later
use minijinja::machinery::{CodeGenerator, Instruction, parse_expr};
use rstest::rstest;

use super::route_calls;
use crate::template::order::OPERATORS;

/// The rewrite names a comparison by the debug form of its instruction, the only name minijinja
/// gives it: a new minijinja that writes it otherwise fails here, not by leaving `<` inexact.
#[rstest]
#[case("x == y", 1)]
#[case("x != y", 1)]
#[case("x < y", 1)]
#[case("x <= y", 1)]
#[case("x > y", 1)]
#[case("x >= y", 1)]
#[case("x < y <= z", 2)]
#[case("x == y != z > w >= v", 4)]
fn every_comparison_is_routed(#[case] expr: &str, #[case] operators: usize) {
    let ast = parse_expr(expr).expect("the expression parses");
    let mut codegen = CodeGenerator::new("<test>", expr);
    codegen.compile_expr(&ast);
    let (mut instructions, _) = codegen.finish();
    let _ = route_calls(&mut instructions, false);
    let mut at = 0;
    let mut routed = 0;
    while let Some(op) = instructions.get(at) {
        assert!(
            !matches!(
                op,
                Instruction::Eq
                    | Instruction::Ne
                    | Instruction::Lt
                    | Instruction::Lte
                    | Instruction::Gt
                    | Instruction::Gte
                    | Instruction::CompareAndPreserve(_)
            ),
            "{expr}: {op:?} is left to minijinja"
        );
        if let Instruction::CallFunction(name, _) = op
            && OPERATORS
                .iter()
                .any(|(_, key, keep)| key == name || keep == name)
        {
            routed += 1;
        }
        at += 1;
    }
    assert_eq!(routed, operators, "{expr}");
}
