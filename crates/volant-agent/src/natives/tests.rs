// SPDX-License-Identifier: GPL-3.0-or-later

use volant_protocol::modules::NATIVE_CANDIDATES;

use super::*;

/// The protocol's list of candidates and the agent's table name the same modules.
#[test]
fn every_candidate_has_a_native_and_every_native_is_a_candidate() {
    for name in NATIVE_CANDIDATES {
        assert!(find(name).is_some(), "{name} has no native");
    }
    for native in NATIVES.iter().filter(|n| !n.name.starts_with("volant_")) {
        for name in std::iter::once(native.name).chain(native.aliases.iter().copied()) {
            assert!(NATIVE_CANDIDATES.contains(&name), "{name} is no candidate");
        }
    }
}

/// Under check or diff mode a native that changes the host hands back, and a read-only one
/// answers. One row per candidate, so a new native has to take a side.
#[rstest::rstest]
#[case::setup("setup", false)]
#[case::stat("stat", false)]
#[case::package_facts("package_facts", false)]
#[case::service_facts("service_facts", false)]
#[case::file("file", true)]
#[case::copy("copy", true)]
#[case::lineinfile("lineinfile", true)]
#[case::systemd("systemd", true)]
#[case::apt("apt", true)]
#[case::user("user", true)]
#[case::group("group", true)]
fn a_native_that_changes_the_host_hands_back_under_check_and_diff(
    #[case] name: &str,
    #[case] hands_back: bool,
) {
    // The rows, plus `systemd_service`, which is `systemd` under another name.
    assert_eq!(NATIVE_CANDIDATES.len(), 11 + 1, "a candidate has no row");
    let Some(native) = find(name) else {
        // Not built on this platform.
        return;
    };
    assert_eq!(mode_hand_back(native, false, false), None);
    for (check, diff) in [(true, false), (false, true), (true, true)] {
        assert_eq!(
            mode_hand_back(native, check, diff).is_some(),
            hands_back,
            "{name}, check {check}, diff {diff}"
        );
    }
}

/// The modules page says which natives run from the protocol's list; this agent's table is the
/// truth.
#[test]
#[cfg(unix)]
fn the_modules_page_names_the_natives_this_agent_runs() {
    use volant_protocol::modules::NATIVE_ENABLED;

    let mut ours = enabled_names();
    ours.retain(|name| name != "volant_echo");
    ours.sort();
    let mut page = NATIVE_ENABLED.to_vec();
    page.sort_unstable();
    assert_eq!(ours, page);
}

/// A hand-back provoked with a secret-looking value, for the reason guard below: the case,
/// the secret, and the reason the native gave (`None` when it did not hand back). A native
/// whose input is a host file or a command's output adds its cases from its own fake host.
#[cfg(target_os = "linux")]
pub(super) type Probe = (&'static str, &'static str, Option<String>);

/// A hand-back reason names the argument and why the native leaves it, never the value.
#[test]
#[cfg(target_os = "linux")]
#[expect(clippy::too_many_lines)]
fn a_hand_back_reason_never_quotes_an_argument_value() {
    use serde_json::json;

    let scratch = common::golden::Scratch::new("reason-values");
    let file = scratch.path("config.yaml");
    std::fs::write(&file, "token: K3S0SECRET0E\n").unwrap();
    std::fs::create_dir(scratch.path(".tmp")).unwrap();
    let context = Context {
        remote_tmp: scratch.path(".tmp"),
        ..Context::default()
    };
    // Natives that take the value as an argument, run as the agent runs them.
    let by_args = [
        (
            "lineinfile",
            json!({"path": file, "regexp": "^token: K3S0SECRET0A**", "line": "x"}),
            "K3S0SECRET0A",
        ),
        (
            "lineinfile",
            json!({"path": file, "regexp": "K3S0SECRET0B*+", "line": "x"}),
            "K3S0SECRET0B",
        ),
        (
            "lineinfile",
            json!({"path": file, "regexp": "(?<K3S0SECRET0C>x)", "line": "x"}),
            "K3S0SECRET0C",
        ),
        (
            "lineinfile",
            json!({"path": file, "insertafter": "K3S0SECRET0D**", "line": "x"}),
            "K3S0SECRET0D",
        ),
        (
            "lineinfile",
            json!({
                "path": file,
                "search_string": "token: K3S0SECRET0E",
                "line": "token: K3S0SECRET0F",
                "validate": "/nonexistent/K3S0SECRET0G %s",
            }),
            "K3S0SECRET0",
        ),
        (
            "copy",
            json!({"src": file, "dest": scratch.path("copied"), "mode": "+75318642"}),
            "75318642",
        ),
        (
            "file",
            json!({"path": file, "state": "K3S0SECRET0H"}),
            "K3S0SECRET0H",
        ),
        (
            "file",
            json!({"path": file, "owner": "+4815162342"}),
            "4815162342",
        ),
        (
            "user",
            json!({"name": "volant-reason-probe", "group": "+4815162343"}),
            "4815162343",
        ),
        (
            "user",
            json!({"name": "volant-reason-probe", "groups": ["+4815162344"]}),
            "4815162344",
        ),
        (
            "group",
            json!({"name": "volant-reason-probe", "gid": "K3S0SECRET0L"}),
            "K3S0SECRET0L",
        ),
        (
            "group",
            json!({"name": "volant-reason-probe", "state": "K3S0SECRET0M"}),
            "K3S0SECRET0M",
        ),
        ("group", json!({"name": "K3S0SECRET0N:x"}), "K3S0SECRET0N"),
        (
            "systemd",
            json!({"name": "volant-reason-probe.service", "state": "K3S0SECRET0O"}),
            "K3S0SECRET0O",
        ),
        (
            "systemd",
            json!({"name": "K3S0SECRET0P.service", "masked": true}),
            "K3S0SECRET0P",
        ),
        (
            "systemd",
            json!({"name": "K3S0SECRET0Q*.service", "state": "started"}),
            "K3S0SECRET0Q",
        ),
        (
            "setup",
            json!({"gather_subset": ["k3s0secret0k"]}),
            "k3s0secret0k",
        ),
    ];
    let mut probes: Vec<Probe> = by_args
        .into_iter()
        .map(|(name, args, secret)| {
            let ran = run(
                find(name).unwrap(),
                args.as_object().unwrap(),
                &context,
                &|| false,
            );
            let reason = match ran {
                NativeRun::Fallback(reason) => Some(reason),
                _ => None,
            };
            (name, secret, reason)
        })
        .collect();
    // Natives that read the value from a host file or a command's output, on a fake host.
    probes.extend(apt::tests::secret_probes());
    probes.extend(package_facts::secret_probes());
    probes.extend(service_facts::secret_probes());

    let mut leaks = Vec::new();
    for (case, secret, reason) in probes {
        match reason {
            Some(reason) if reason.contains(secret) => leaks.push(format!("{case}: {reason}")),
            Some(_) => {}
            None => leaks.push(format!("{case} did not hand back on {secret}")),
        }
    }
    assert!(leaks.is_empty(), "reasons quote a value: {leaks:#?}");
}
