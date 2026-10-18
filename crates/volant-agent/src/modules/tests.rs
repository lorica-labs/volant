// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;
use volant_protocol::modules::{COMMAND, NATIVE_MODULES, RAW, SHELL};

use super::*;

#[test]
fn every_registered_module_is_implemented_and_nothing_more() {
    let registered: Vec<&str> = NATIVE_MODULES.iter().map(|m| m.name).collect();
    let implemented: Vec<&str> = MODULES.iter().map(|m| m.spec.name).collect();
    assert_eq!(implemented, registered);
}

/// The sentence and the module name are the reference's, not this engine's, and `shell` is
/// answered under `command` because that is the module the reference runs it with.
#[test]
fn an_argument_the_reference_does_not_have_gets_the_reference_s_sentence() {
    use crate::testing::args;
    for spec in [&COMMAND, &SHELL] {
        let msg = unsupported_parameters(
            spec,
            &args(json!({"_raw_params": "/bin/true", "no_such_arg": 1})),
        )
        .expect("an argument nobody has is refused");
        assert_eq!(
            msg,
            "Unsupported parameters for (ansible.legacy.command) module: no_such_arg. Supported parameters include: _raw_params, _uses_shell, argv, chdir, cmd, creates, executable, expand_argument_vars, removes, stdin, stdin_add_newline, strip_empty_ends."
        );
        assert_eq!(
            unsupported_parameters(spec, &args(json!({"_raw_params": "x", "chdir": "/tmp"}))),
            None,
            "an argument the reference has passes"
        );
    }
    assert_eq!(
        unsupported_parameters(&RAW, &args(json!({"no_such_arg": 1}))),
        None,
        "raw takes any argument without looking at it"
    );
}

/// `shell` refuses `expand_argument_vars` whatever its value, a template the pre-flight could not
/// read included; `command` reads it.
#[test]
fn a_refused_argument_whose_value_arrives_late_fails_the_task() {
    use crate::testing::args;
    for any in [json!(true), json!(false), json!("no"), json!("maybe")] {
        assert_eq!(
            refused_argument(&SHELL, &args(json!({"expand_argument_vars": any.clone()})))
                .as_deref(),
            Some(
                "argument 'expand_argument_vars' is not supported yet on 'shell'. The host renders this value, so the check before the run could not read it."
            ),
            "{any}"
        );
    }
    assert_eq!(
        refused_argument(&COMMAND, &args(json!({"expand_argument_vars": true}))),
        None
    );
    for spec in [&COMMAND, &SHELL] {
        assert_eq!(
            refused_argument(spec, &args(json!({"_raw_params": "x", "chdir": "/tmp"}))),
            None,
            "an argument this release honours is not refused"
        );
    }
    assert_eq!(
        refused_argument(&RAW, &args(json!({"expand_argument_vars": true}))),
        None,
        "raw has no registry entry for it, and the reference takes it without looking"
    );
}
