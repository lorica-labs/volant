// SPDX-License-Identifier: GPL-3.0-or-later
//! The native `setup` on the machine that runs the test, through the agent binary.
//!
//! The values are compared with the reference elsewhere, live, on each machine: facts depend on
//! the machine, so only the reference running there knows them. What this file holds is the key
//! set: the native produces every key of `NATIVE_FACT_KEYS` whose source this machine has, and no
//! key outside the list.

#[cfg(target_os = "linux")]
use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::process::Command;

#[cfg(target_os = "linux")]
use serde_json::Value;
use volant_protocol::facts::NATIVE_FACT_KEYS;

fn agent(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_volant-agent"))
        .args(args)
        .output()
        .expect("the agent runs")
}

#[cfg(target_os = "linux")]
mod setup_exits;

/// The native's answer to `gather_subset: min` on this machine, or `None` once it has handed
/// back for a host outside its subset, which is asserted and printed. Any other hand-back, or a
/// failure, is red. Linux only: the native answers nowhere else.
#[cfg(target_os = "linux")]
fn answer_here() -> Option<Value> {
    let out = agent(&["--native-facts", "min", "python3"]);
    if out.status.success() {
        return Some(serde_json::from_slice(&out.stdout).expect("the answer is JSON"));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let reason = stderr
        .trim()
        .strip_prefix("volant-agent: the native setup hands back: ")
        .unwrap_or_else(|| panic!("the native setup failed without handing back: {stderr}"));
    assert!(
        setup_exits::is_host_exit(reason),
        "the native setup handed back on this machine for a reason other than a host outside \
         its subset: {reason}"
    );
    eprintln!("this machine is outside the native setup's subset ({reason}): nothing to compare");
    None
}

/// The keys whose source this machine lacks, which the reference leaves out as well.
#[cfg(target_os = "linux")]
fn absent_here() -> BTreeSet<&'static str> {
    let mut absent = BTreeSet::new();
    let ssh_key = |algo: &str| {
        ["/etc/ssh", "/etc/openssh", "/etc"]
            .iter()
            .any(|dir| Path::new(&format!("{dir}/ssh_host_{algo}_key.pub")).exists())
    };
    for algo in ["dsa", "rsa", "ecdsa", "ed25519"] {
        if !ssh_key(algo) {
            for key in NATIVE_FACT_KEYS {
                if key.starts_with(&format!("ansible_ssh_host_key_{algo}_")) {
                    absent.insert(*key);
                }
            }
        }
    }
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    if !os_release.contains("Debian") && !os_release.contains("Raspbian") {
        absent.insert("ansible_distribution_minor_version");
    }
    let non_empty = |path: &str| std::fs::read_to_string(path).is_ok_and(|s| !s.trim().is_empty());
    if !non_empty("/var/lib/dbus/machine-id") && !non_empty("/etc/machine-id") {
        absent.insert("ansible_machine_id");
    }
    if !non_empty("/proc/cmdline") {
        absent.insert("ansible_cmdline");
        absent.insert("ansible_proc_cmdline");
    }
    // The reference writes it for an x86 machine only.
    let machine = Command::new("uname")
        .arg("-m")
        .output()
        .expect("uname runs")
        .stdout;
    let machine = String::from_utf8_lossy(&machine);
    let machine = machine.trim();
    let x86 = machine == "x86_64"
        || ["i386", "i486", "i586", "i686", "i86pc"]
            .iter()
            .any(|i86| machine.contains(i86));
    if !x86 {
        absent.insert("ansible_userspace_architecture");
    }
    absent
}

/// On this machine the native answers `gather_subset: min` with exactly the keys of
/// `NATIVE_FACT_KEYS` whose source exists here, each under the name the reference gives it.
///
/// What would make this red: a key the collector writes and the list does not name, which the
/// controller would never know it can rely on; a key the list names and the collector never
/// writes, which a play reading it would find missing; or the native handing back for anything
/// but a host outside its subset. On such a host (a runner with `/usr/bin/rpm`) it asserts the
/// hand-back's reason and compares nothing.
#[test]
#[cfg(target_os = "linux")]
fn the_native_setup_produces_exactly_the_listed_keys_on_this_machine() {
    let Some(result) = answer_here() else {
        return;
    };
    let produced: BTreeSet<&str> = result["ansible_facts"]
        .as_object()
        .expect("the facts are an object")
        .keys()
        .map(String::as_str)
        .collect();
    let absent = absent_here();
    let expected: BTreeSet<&str> = NATIVE_FACT_KEYS
        .iter()
        .copied()
        .filter(|key| !absent.contains(key))
        .collect();
    assert_eq!(
        produced.difference(&expected).collect::<Vec<_>>(),
        Vec::<&&str>::new(),
        "produced and not listed"
    );
    assert_eq!(
        expected.difference(&produced).collect::<Vec<_>>(),
        Vec::<&&str>::new(),
        "listed, with a source on this machine, and not produced"
    );
    assert_eq!(
        result["ansible_facts"]["gather_subset"],
        serde_json::json!(["min"])
    );
    assert_eq!(
        result["invocation"],
        serde_json::json!({"module_args": {
            "gather_subset": ["min"],
            "gather_timeout": 10,
            "filter": [],
            "fact_path": "/etc/ansible/facts.d",
        }})
    );
}

/// The live comparison leaves `date_time` out because its values move, so its zone fields are
/// compared here against Python's `time` module on the same machine, where the native answers.
///
/// What would make this red: `tz_dst` read from the current offset rather than the zone's
/// daylight name, or `tz_offset` printed without the sign and four digits of `%z`.
#[test]
#[cfg(target_os = "linux")]
fn the_time_zone_names_match_python_on_this_machine() {
    let Some(result) = answer_here() else {
        return;
    };
    let date_time = &result["ansible_facts"]["ansible_date_time"];
    let python = Command::new("python3")
        .arg("-c")
        .arg("import time; print(time.strftime('%Z'), time.tzname[1], time.strftime('%z'))")
        .output()
        .expect("python3 runs");
    assert_eq!(
        String::from_utf8_lossy(&python.stdout).trim(),
        format!(
            "{} {} {}",
            date_time["tz"].as_str().unwrap(),
            date_time["tz_dst"].as_str().unwrap(),
            date_time["tz_offset"].as_str().unwrap()
        )
    );
}

/// `--native-fact-keys` prints the list, one key per line, for the live comparison to read.
#[test]
fn the_fact_keys_are_printed_one_per_line() {
    let out = agent(&["--native-fact-keys"]);
    assert!(out.status.success());
    let printed = String::from_utf8(out.stdout).expect("utf-8");
    assert_eq!(printed.lines().collect::<Vec<_>>(), NATIVE_FACT_KEYS);
    assert!(!NATIVE_FACT_KEYS.is_empty());
}
