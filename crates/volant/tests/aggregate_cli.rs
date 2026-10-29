// SPDX-License-Identifier: GPL-3.0-or-later
//! The callbacks that print beside the stdout one: `timer` and `profile_tasks` of ansible.posix,
//! and `tree`. Each run is compared with `ansible-playbook`'s, its clocks masked. Skipped without
//! the recorded ansible-core and ansible.posix.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

#[path = "common/python.rs"]
mod python;

const PLAY: &str = r#"- hosts: all
  gather_facts: false
  tasks:
    - name: first
      command: echo hi
    - name: second
      debug:
        msg: two
    - name: third
      command: "false"
      ignore_errors: true
"#;

/// ansible.posix on the collections path.
fn posix() -> bool {
    let found = std::env::var_os("ANSIBLE_COLLECTIONS_PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .any(|path| path.join("ansible_collections/ansible/posix").is_dir())
    });
    if !found {
        eprintln!("skipped: ansible.posix on ANSIBLE_COLLECTIONS_PATH is needed");
    }
    found
}

/// The play under `program` in `dir`, with `env`, one fork so the hosts keep their order.
fn run(program: &[&Path], dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let out = Command::new(program[0])
        .args(&program[1..])
        .args(["-i", "inv.ini", "play.yml", "-f", "1"])
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("ANSIBLE_NOCOLOR", "1")
        .env("ANSIBLE_CONFIG", dir.join("ansible.cfg"))
        .envs(env.iter().copied())
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `text` with its clocks masked: the `profile_tasks` line, a task's seconds, the timer's seconds
/// and the times a command result carries. The version block `-vv` prints is the engine's own.
fn masked(text: &str) -> String {
    let mut out = Vec::new();
    let mut started = false;
    for line in text.lines() {
        started |= line.starts_with("Using ") || !text.contains("Using ");
        if !started {
            continue;
        }
        let mut line = line.to_string();
        if let Some(at) = line.find(" (0:") {
            if line.ends_with("* ") {
                line = format!("<time>{}", &line[at..at + 1]);
            }
        }
        if line.ends_with('s')
            && let Some((head, tail)) = line.rsplit_once(' ')
            && tail.trim_end_matches('s').parse::<f64>().is_ok()
        {
            line = format!("{head} <s>");
        }
        if line.starts_with("Playbook run took") {
            line = "Playbook run took <time>".to_string();
        }
        for key in ["delta", "end", "start"] {
            let needle = format!("\"{key}\": \"");
            while let Some(at) = line.find(&needle) {
                let from = at + needle.len();
                let to = from + line[from..].find('"').unwrap_or(0);
                line.replace_range(at..=to, &format!("\"{key}\": <time>"));
            }
        }
        out.push(line);
    }
    out.join("\n")
}

fn setup(dir: &Path, python: &Path) {
    std::fs::write(
        dir.join("inv.ini"),
        format!(
            "h1 ansible_connection=local ansible_python_interpreter={0}\n\
             h2 ansible_connection=local ansible_python_interpreter={0}\n",
            python.display()
        ),
    )
    .unwrap();
    std::fs::write(dir.join("play.yml"), PLAY).unwrap();
    std::fs::write(
        dir.join("ansible.cfg"),
        "[callback_profile_tasks]\nsort_order = none\n",
    )
    .unwrap();
}

/// `timer` and `profile_tasks` together, in that order, at 0 and `-vv`: the time line under each
/// banner, the tasks recap, the run's time, each where the reference puts it.
#[test]
fn timer_and_profile_tasks_print_as_under_ansible_playbook() {
    let Some(python) = python::reference_python() else {
        return;
    };
    if !posix() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), &python);
    let reference = python.parent().unwrap().join("ansible-playbook");
    let volant = Path::new(env!("CARGO_BIN_EXE_volant"));
    let env = [
        (
            "ANSIBLE_CALLBACKS_ENABLED",
            "ansible.posix.timer,profile_tasks",
        ),
        ("VOLANT_PYTHON", python.to_str().unwrap()),
    ];
    for args in [&[][..], &["-vv"]] {
        let want = masked(&run(&[&reference], dir.path(), args, &env));
        let got = masked(&run(
            &[volant, Path::new("playbook")],
            dir.path(),
            args,
            &env,
        ));
        assert_eq!(got, want, "at {args:?}");
        assert!(want.contains("TASKS RECAP"), "{want}");
        assert!(want.contains("PLAYBOOK RECAP"), "{want}");
    }
}

/// `summary_only` keeps the lines under the banners off, and `output_limit` cuts the recap.
#[test]
fn profile_tasks_reads_its_options() {
    let Some(python) = python::reference_python() else {
        return;
    };
    if !posix() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), &python);
    let reference = python.parent().unwrap().join("ansible-playbook");
    let volant = Path::new(env!("CARGO_BIN_EXE_volant"));
    let env = [
        ("ANSIBLE_CALLBACKS_ENABLED", "ansible.posix.profile_tasks"),
        ("PROFILE_TASKS_SUMMARY_ONLY", "true"),
        ("PROFILE_TASKS_TASK_OUTPUT_LIMIT", "2"),
        ("VOLANT_PYTHON", python.to_str().unwrap()),
    ];
    let want = masked(&run(&[&reference], dir.path(), &[], &env));
    let got = masked(&run(
        &[volant, Path::new("playbook")],
        dir.path(),
        &[],
        &env,
    ));
    assert_eq!(got, want);
    assert!(!want.contains("third --"), "{want}");
}

/// `tree` writes each host's last result in the directory `ANSIBLE_CALLBACK_TREE_DIR` names.
#[test]
fn tree_writes_each_host_s_last_result() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), &python);
    let reference = python.parent().unwrap().join("ansible-playbook");
    let volant = Path::new(env!("CARGO_BIN_EXE_volant"));
    let python_text = python.to_str().unwrap();
    for (program, tree) in [
        (vec![reference.as_path()], "tree-reference"),
        (vec![volant, Path::new("playbook")], "tree-volant"),
    ] {
        let tree_dir = dir.path().join(tree);
        let env = [
            ("ANSIBLE_CALLBACKS_ENABLED", "tree"),
            ("ANSIBLE_CALLBACK_TREE_DIR", tree_dir.to_str().unwrap()),
            ("VOLANT_PYTHON", python_text),
        ];
        run(&program, dir.path(), &[], &env);
    }
    for host in ["h1", "h2"] {
        let read = |tree: &str| {
            masked(&std::fs::read_to_string(dir.path().join(tree).join(host)).unwrap())
        };
        assert_eq!(read("tree-volant"), read("tree-reference"), "{host}");
    }
}

const JUNIT_PLAY: &str = r#"- hosts: all
  gather_facts: false
  tasks:
    - name: first
      command: echo hi
    - name: second
      debug:
        msg: two
    - name: third
      command: "false"
      ignore_errors: true
    - name: skipped one
      command: echo no
      when: false
    - name: hidden
      command: echo secret
      no_log: true
    - name: looped
      command: "echo {{ item }}"
      loop: [a, b]
    - name: a module fails
      command: "false"
      when: inventory_hostname == 'h2'
    - name: an action fails
      fail:
        msg: boom
"#;

/// The XML file `junit` wrote in `dir`, its times masked.
fn junit_report(dir: &Path) -> String {
    let files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("play-") && name.ends_with(".xml"),
        "{name}"
    );
    let text = std::fs::read_to_string(&files[0]).unwrap();
    let mut out = String::new();
    for line in masked(&text).lines() {
        let mut line = line.to_string();
        while let Some(at) = line.find(" time=\"") {
            let from = at + 7;
            let to = from + line[from..].find('"').unwrap_or(0);
            line.replace_range(at..=to, " TIME");
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// `junit` writes the report `ansible-playbook` writes: a test case per task and host, an ignored
/// failure passing, a skipped task and a real failure as the reference files them.
#[test]
fn junit_writes_the_report_ansible_playbook_writes() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), &python);
    std::fs::write(dir.path().join("play.yml"), JUNIT_PLAY).unwrap();
    let reference = python.parent().unwrap().join("ansible-playbook");
    let volant = Path::new(env!("CARGO_BIN_EXE_volant"));
    let python_text = python.to_str().unwrap();
    for (program, report) in [
        (vec![reference.as_path()], "junit-reference"),
        (vec![volant, Path::new("playbook")], "junit-volant"),
    ] {
        let report_dir = dir.path().join(report);
        let env = [
            ("ANSIBLE_CALLBACKS_ENABLED", "junit"),
            ("JUNIT_OUTPUT_DIR", report_dir.to_str().unwrap()),
            ("VOLANT_PYTHON", python_text),
        ];
        run(&program, dir.path(), &[], &env);
    }
    let want = junit_report(&dir.path().join("junit-reference"));
    assert_eq!(junit_report(&dir.path().join("junit-volant")), want);
    assert!(
        want.contains("<failure message=\"non-zero return code\""),
        "{want}"
    );
}
