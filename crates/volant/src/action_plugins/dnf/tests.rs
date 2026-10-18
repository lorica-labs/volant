// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// A delegated task's result carries no `pkg_mgr`, because its facts would be filed under the
/// wrong host.
#[test]
fn a_delegated_task_carries_no_pkg_mgr() {
    let args = Map::new();
    let running = crate::vars::HostVars::default();
    for (delegated, facts) in [(false, Some(json!({"pkg_mgr": "dnf5"}))), (true, None)] {
        let mut plugin = Dnf {
            args: &args,
            running_vars: &running,
            delegated,
            state: State::Start,
            pkg_mgr: None,
        };
        assert!(matches!(
            plugin.next(None),
            Step::Run(Sub {
                module: "setup",
                ..
            })
        ));
        let setup = TaskResult(
            json!({"ansible_facts": {"ansible_pkg_mgr": "dnf5"}})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        assert!(matches!(
            plugin.next(Some(setup)),
            Step::Run(Sub { module: "dnf5", .. })
        ));
        let Step::Done(result) = plugin.next(Some(TaskResult(Map::new()))) else {
            panic!("the module's result ends the item");
        };
        assert_eq!(result.0.get("ansible_facts").cloned(), facts, "{delegated}");
    }
}
