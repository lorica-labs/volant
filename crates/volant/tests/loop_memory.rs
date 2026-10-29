// SPDX-License-Identifier: GPL-3.0-or-later
//! The memory of a loop grows with the data of its items, not with the host's variables, and the
//! memory of a play with the data its hosts add, not with its own variables times its hosts.

#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The peak resident memory, in MB, of a run of the playbook on `hosts` local hosts, read from
/// `/proc` while it runs.
fn peak_megabytes(dir: &std::path::Path, playbook: &str, hosts: usize) -> u64 {
    std::fs::write(dir.join("p.yml"), playbook).expect("the playbook is written");
    let mut inventory = String::from("[all]\n");
    for n in 1..=hosts {
        writeln!(inventory, "h{n} ansible_connection=local").expect("a String takes any write");
    }
    std::fs::write(dir.join("hosts.ini"), inventory).expect("the inventory is written");
    let mut child = Command::new(env!("CARGO_BIN_EXE_volant"))
        .args(["playbook", "-i", "hosts.ini", "p.yml"])
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("VOLANT_RUNTIME", "system")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("volant starts");
    let started = Instant::now();
    let mut peak_kb = 0;
    loop {
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", child.id()))
            && let Some(line) = status.lines().find(|l| l.starts_with("VmHWM:"))
        {
            let kb: u64 = line
                .split_whitespace()
                .nth(1)
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            peak_kb = peak_kb.max(kb);
        }
        if let Some(status) = child.try_wait().expect("the child is polled") {
            assert!(status.success(), "the run failed: {status}");
            // A run that ends before the first read would otherwise pass any bound.
            assert!(
                peak_kb > 0,
                "no memory reading was taken while the run lasted"
            );
            return peak_kb / 1024;
        }
        if started.elapsed() > Duration::from_secs(300) {
            let _ = child.kill();
            panic!("the run did not end");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The bound is for a debug build, which the tests use: a release build peaks lower. The runs it
/// fences off are an order of magnitude apart (about 1 GB before the fix, under 100 MB after).
const DEBUG_BUILD_BOUND_MB: u64 = 250;

/// A loop walks the `LINES` lines a command printed. Each item used to own a copy of the host's
/// variables, `r` among them, so the memory was the number of items times the result's size.
#[test]
fn a_loop_over_a_long_result_does_not_copy_it_for_every_item() {
    const LINES: usize = 3000;
    const BOUND_MB: u64 = DEBUG_BUILD_BOUND_MB;
    let dir = std::env::temp_dir().join(format!("volant-loop-memory-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the directory is made");
    let playbook = format!(
        "- hosts: h1\n  gather_facts: false\n  tasks:\n    - command: seq 1 {LINES}\n      register: r\n    - debug: {{msg: \"{{{{ item }}}}\"}}\n      loop: \"{{{{ r.stdout_lines }}}}\"\n      loop_control: {{label: x}}\n"
    );
    let peak = peak_megabytes(&dir, &playbook, 1);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        peak < BOUND_MB,
        "a loop of {LINES} items over a {LINES}-line result peaked at {peak} MB, the bound is {BOUND_MB} MB"
    );
}

/// An extended loop hands every item the list of all the items: one list for the loop, not one
/// copy per item, and each item still reads the whole list and its own place in it.
#[test]
fn an_extended_loop_over_a_long_result_shares_the_list_of_all_items() {
    const LINES: usize = 3000;
    let dir = std::env::temp_dir().join(format!("volant-loop-extended-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the directory is made");
    let playbook = format!(
        "- hosts: h1\n  gather_facts: false\n  tasks:\n    - command: seq 1 {LINES}\n      register: r\n    - debug: {{msg: \"{{{{ item }}}}\"}}\n      loop: \"{{{{ r.stdout_lines }}}}\"\n      loop_control: {{label: x, extended: true}}\n      failed_when: (ansible_loop.allitems | length) != {LINES} or ansible_loop.allitems[ansible_loop.index0] != item\n"
    );
    let peak = peak_megabytes(&dir, &playbook, 1);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        peak < DEBUG_BUILD_BOUND_MB,
        "an extended loop of {LINES} items peaked at {peak} MB, the bound is {DEBUG_BUILD_BOUND_MB} MB"
    );
}

/// Every host of a play reads its `vars_files` through one copy of them. Each host used to hold
/// its own, twice, and its tasks one more each: 50 hosts reading a 7 MB file peaked at 5 GB.
#[test]
fn the_hosts_of_a_play_share_its_vars_files() {
    const ROWS: usize = 20_000;
    const HOSTS: usize = 40;
    let dir = std::env::temp_dir().join(format!("volant-vars-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the directory is made");
    let rows: Vec<String> = (0..ROWS)
        .map(|n| {
            format!(r#"{{"id": {n}, "name": "name-{n}", "text": "lorem ipsum dolor sit amet"}}"#)
        })
        .collect();
    std::fs::write(
        dir.join("big.json"),
        format!(r#"{{"rows": [{}]}}"#, rows.join(", ")),
    )
    .expect("the vars file is written");
    let playbook = format!(
        "- hosts: all\n  gather_facts: false\n  vars_files: [big.json]\n  tasks:\n    - debug: {{msg: \"{{{{ rows | length }}}}\"}}\n      failed_when: (rows | length) != {ROWS}\n    - debug: {{msg: \"{{{{ rows | length }}}}\"}}\n"
    );
    let one = peak_megabytes(&dir, &playbook, 1);
    let many = peak_megabytes(&dir, &playbook, HOSTS);
    let _ = std::fs::remove_dir_all(&dir);
    // Measured on a debug build: about 130 MB for one host, 310 MB for 40 hosts with the copy
    // shared, and 845 MB for only 20 without.
    assert!(
        many < 3 * one,
        "{HOSTS} hosts reading the same vars_files peaked at {many} MB, one host at {one} MB"
    );
}
