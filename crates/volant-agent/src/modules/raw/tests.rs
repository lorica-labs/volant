// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[test]
fn raw_reports_only_what_ansible_reports() {
    let args: Map<String, Value> = json!({"_raw_params": "echo $((2 + 2))"})
        .as_object()
        .unwrap()
        .clone();
    let Run::Done(r) = run_raw(&args, &Context::default(), &|| false) else {
        panic!("cancelled")
    };
    // Ansible keeps the trailing newline for `raw` where `command` strips it.
    assert_eq!(r.0["stdout"], "4\n");
    assert_eq!(r.0["stdout_lines"], json!(["4"]));
    assert_eq!(r.0["rc"], 0);
    assert!(r.changed());
    for absent in ["cmd", "start", "end", "delta", "msg"] {
        assert!(
            !r.0.contains_key(absent),
            "{absent} must not be reported by raw"
        );
    }
}

#[test]
fn raw_failure_is_carried_by_rc_alone() {
    let check = Context {
        check_mode: true,
        ..Context::default()
    };
    let args: Map<String, Value> = json!({"_raw_params": "exit 3"})
        .as_object()
        .unwrap()
        .clone();
    let Run::Done(r) = run_raw(&args, &check, &|| false) else {
        panic!("cancelled")
    };
    assert_eq!(
        Value::Object(r.0),
        json!({"changed": false, "skipped": true}),
        "check mode skips raw"
    );
    let args: Map<String, Value> = json!({"_raw_params": "exit 3"})
        .as_object()
        .unwrap()
        .clone();
    let Run::Done(r) = run_raw(&args, &Context::default(), &|| false) else {
        panic!("cancelled")
    };
    assert_eq!(r.0["rc"], 3);
    assert!(r.failed());
}

#[test]
fn executable_selects_the_shell() {
    let args: Map<String, Value> = json!({"_raw_params": "echo $0", "executable": "/bin/sh"})
        .as_object()
        .unwrap()
        .clone();
    let Run::Done(r) = run_raw(&args, &Context::default(), &|| false) else {
        panic!("cancelled")
    };
    assert_eq!(r.0["stdout"], "/bin/sh\n");
}
