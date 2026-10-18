// SPDX-License-Identifier: GPL-3.0-or-later

use super::{FATAL, RUN_ONCE, bypass_error, loosen};

fn compiled(text: &str) -> crate::compile::Compiled {
    let pb = crate::playbook::parse(text, "play.yml").unwrap_or_else(|e| panic!("{e:#}"));
    let search = crate::roles::RoleSearch {
        paths: Vec::new(),
        collections: Vec::new(),
    };
    crate::compile::compile(
        &pb.plays[0],
        &search,
        &crate::compile::TagSelection::default(),
    )
    .unwrap_or_else(|e| panic!("{e:#}"))
}

/// `run_once` and `any_errors_fatal` are cleared on tasks and handlers, each warned about once,
/// in the order the tasks come.
#[test]
fn loosen_clears_the_meeting_keywords_and_warns_once_each() {
    let mut c = compiled(
        "- hosts: all\n  gather_facts: false\n  any_errors_fatal: true\n  tasks:\n    - debug: msg=a\n      run_once: true\n    - debug: msg=b\n      run_once: true\n  handlers:\n    - name: h\n      debug: msg=h\n      run_once: true\n",
    );
    assert_eq!(loosen(&mut c), [FATAL, RUN_ONCE]);
    let tasks = c.steps.iter().map(|s| &s.task);
    for task in tasks.chain(c.handlers.iter().map(|h| &h.task)) {
        assert!(!task.runs_once() && !task.fails_all(), "{}", task.name);
    }
    assert!(
        loosen(&mut compiled(
            "- hosts: all\n  tasks:\n    - debug: msg=a\n"
        ))
        .is_empty()
    );
}

/// `pause` and `add_host` end the run with the reference's sentence; any other action does not.
#[test]
fn only_the_actions_that_bypass_the_host_loop_end_the_run() {
    let c = compiled(
        "- hosts: all\n  gather_facts: false\n  tasks:\n    - pause: {seconds: 1}\n    - ansible.builtin.add_host: {name: x}\n    - debug: msg=a\n",
    );
    let errors: Vec<Option<String>> = c.steps.iter().map(|s| bypass_error(&s.task)).collect();
    let first = errors[0].as_deref().expect("pause ends the run");
    assert!(first.starts_with("The 'pause' module bypasses the host loop, which is currently not supported in the free strategy and would instead execute for every host in the inventory list.\nOrigin: "), "{first}");
    assert!(first.contains("play.yml:4:7\n"), "{first}");
    let second = errors[1].as_deref().expect("add_host ends the run");
    assert!(
        second.starts_with("The 'ansible.builtin.add_host' module"),
        "{second}"
    );
    assert_eq!(errors[2], None);
}
