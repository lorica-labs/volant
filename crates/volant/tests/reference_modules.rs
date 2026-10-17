// SPDX-License-Identifier: GPL-3.0-or-later
//! The `module` goldens: the recorded module play replayed under Volant on this machine, which
//! needs the recorded ansible-core, `sudo -n`, systemd, apt and cron. The `ssh` job runs them.
#![cfg(target_os = "linux")]

use rstest::rstest;
use serde_json::{Map, Value};

#[path = "common/collections.rs"]
mod collections;

#[path = "common/compare.rs"]
#[expect(dead_code, reason = "reference.rs uses the rest")]
mod compare;
use compare::{
    check_native_path, compare_native, compare_shape, failed_tasks, keep_live, module_file,
    native_lock, native_profile, native_result, redact_staged, replace_in_strings, results_by_task,
    same, same_names, shape_result, unit_after,
};

/// The reasons the native `setup` hands back for a host outside its subset, the agent's own list.
#[path = "../../volant-agent/tests/setup_exits/mod.rs"]
mod setup_exits;

#[path = "common/process.rs"]
mod process;

#[path = "common/python.rs"]
mod python;

/// The module play is the longest run: under a minute on the development machine.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// The generator's own inputs of the action plugin cases, which `play.json` names as
/// `<golden-action-src>`.
const ACTION_SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/action-src");

/// `ANSIBLE_COLLECTIONS_PATHS`/`ANSIBLE_COLLECTIONS_PATH` from this test process's own
/// environment, to forward through `run_recorded_play` to the child it otherwise strips along
/// with every other `ANSIBLE_*`.
///
/// The `ssh` job installs the pinned collections under its own job directory and sets this
/// variable to it, outside every path `default_collections_path` (`config.rs`) tries on its own.
/// Stripping it unconditionally made the collection golden pass on the dev machine, where the
/// pinned collections also happen to sit on that default path, for a reason that does not hold in
/// the job: there, Volant would search only the empty defaults and find nothing.
#[cfg(target_os = "linux")]
fn collections_path_env() -> Vec<(&'static str, String)> {
    ["ANSIBLE_COLLECTIONS_PATHS", "ANSIBLE_COLLECTIONS_PATH"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name, value)))
        .collect()
}

/// Runs `playbook`, already written under `dir`, against `localhost` over the local connection,
/// with `flags` (`-v` for the module goldens), and returns once it exits or panics at a deadline.
///
/// `extra_env` is set on the child after every `ANSIBLE_*` and `VOLANT_*` variable is stripped
/// from it, so a caller can forward the one or two names it actually needs
/// (`collections_path_env()`) without reopening the door to the rest of this process's own
/// environment: a `VOLANT_NATIVE_MODULES=0` in the shell would otherwise send every task to Python.
#[cfg(target_os = "linux")]
fn run_recorded_play(
    dir: &std::path::Path,
    playbook: &str,
    python: &std::path::Path,
    flags: &[&str],
    extra_env: &[(&str, String)],
) -> std::process::Output {
    std::fs::write(
        dir.join("hosts.ini"),
        "localhost ansible_connection=local\n",
    )
    .expect("the inventory is written");
    let remote_tmp = dir.join("tmp");
    std::fs::create_dir_all(&remote_tmp).expect("the blob cache directory is writable");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .arg("playbook")
        .args(["-i", &dir.join("hosts.ini").display().to_string()])
        .args(flags)
        .arg(dir.join(playbook))
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        // The local agent inherits this run's environment and caches the payload under this
        // directory (`VOLANT_REMOTE_TMP`, set below), so the wipe before the run takes the cache
        // with it rather than leaving it in the shared one.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (name, _) in std::env::vars() {
        if name.starts_with("ANSIBLE_") || name.starts_with("VOLANT_") {
            command.env_remove(name);
        }
    }
    command
        .env("VOLANT_PYTHON", python)
        .env("VOLANT_REMOTE_TMP", &remote_tmp);
    for (name, value) in extra_env {
        command.env(name, value);
    }
    process::run_within(&mut command, DEADLINE, None)
}

/// Where `natives()` recorded, fixed for the reason `PYTHON_MODULES_DIR` is: paths, messages and
/// `_after` carry it, and are compared once both sides read `<golden-tmp>`. Under `/var/tmp`, as
/// in the generator: on the root filesystem rather than a tmpfs, `stat`'s `attributes` and a
/// directory's `size` are the same on every machine these run on, and are compared by value.
#[cfg(target_os = "linux")]
const NATIVE_DIR: &str = "/var/tmp/volant-golden-native";

/// What `natives()` does to a result before recording it, done here to Volant's result and to a
/// live reference result alike: staged paths, then `NATIVE_DIR` and this machine's name in every
/// string, then the running account's own name, group, uid and gid under the ownership keys.
#[cfg(target_os = "linux")]
struct NativeMasks {
    staged_root: String,
    host: String,
    accounts: [(&'static str, Value, &'static str); 6],
}

#[cfg(target_os = "linux")]
impl NativeMasks {
    fn new(staged_root: String) -> Self {
        let id = compare::identity();
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .expect("the host name is readable")
            .trim()
            .to_string();
        Self {
            staged_root,
            host,
            accounts: [
                ("owner", id.user.clone(), "<user>"),
                ("pw_name", id.user, "<user>"),
                ("group", id.group.clone(), "<group>"),
                ("gr_name", id.group, "<group>"),
                ("uid", id.uid, "<uid>"),
                ("gid", id.gid, "<gid>"),
            ],
        }
    }

    fn apply(&self, value: &mut Value) {
        redact_staged(value, &self.staged_root);
        replace_in_strings(value, NATIVE_DIR, "<golden-tmp>");
        replace_in_strings(value, &self.host, "<golden-generator-host>");
        self.accounts_in(value);
    }

    /// Only a value equal to the running account's own is replaced: `root`, `0` and the synthetic
    /// account stay literal, so a native answering the wrong account, or `pw_name` where `gr_name`
    /// belongs, still differs once both sides are masked.
    fn accounts_in(&self, value: &mut Value) {
        match value {
            Value::Array(items) => items.iter_mut().for_each(|v| self.accounts_in(v)),
            Value::Object(map) => {
                for (key, v) in map.iter_mut() {
                    match self
                        .accounts
                        .iter()
                        .find(|(name, real, _)| name == key && v == real)
                    {
                        Some((_, _, placeholder)) => *v = Value::from(*placeholder),
                        None => self.accounts_in(v),
                    }
                }
            }
            _ => {}
        }
    }
}

/// The reference's own results for the recorded `play`, run now on this machine the way
/// `natives()` runs it, keyed by task name: the whole play, so each `live` case meets the state
/// the cases before it left, as it did under Volant.
///
/// With the environment Volant had: no `ANSIBLE_*` or `VOLANT_*` of this process, and the same
/// empty `ANSIBLE_CONFIG`.
#[cfg(target_os = "linux")]
fn reference_results(
    python: &std::path::Path,
    work: &std::path::Path,
    play: &Value,
) -> Map<String, Value> {
    let path = work.join("reference.yml");
    std::fs::write(&path, play.to_string()).expect("the reference play is written");
    let mut command = std::process::Command::new(python);
    command
        .args(["-m", "ansible", "playbook", "-i", "localhost,"])
        .arg(&path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (name, _) in std::env::vars() {
        if name.starts_with("ANSIBLE_") || name.starts_with("VOLANT_") {
            command.env_remove(name);
        }
    }
    command
        .env("ANSIBLE_CONFIG", work.join("ansible.cfg"))
        // The recorder's own callback: `ansible.builtin.json` is `ansible.posix`'s in
        // ansible-core 2.19, which a runner with ansible-core alone does not have.
        .env(
            "ANSIBLE_CALLBACK_PLUGINS",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/callback_plugins"),
        )
        .env("ANSIBLE_STDOUT_CALLBACK", "golden_json")
        .env("ANSIBLE_NOCOLOR", "1")
        .envs(collections_path_env());
    let out = process::run_within(&mut command, DEADLINE, None);
    assert!(
        out.status.success(),
        "the reference failed on the recorded play:
{}
{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).expect("the JSON callback's report");
    report["plays"][0]["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|task| {
            Some((
                task["task"]["name"].as_str()?.to_string(),
                task["hosts"]["localhost"].clone(),
            ))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn succeeds(probe: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", probe])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// `module/play.json` under Volant at `-vvv`: each case against its recording or a live reference
/// run, by its index entry, and a native case on the path the index names.
#[cfg(target_os = "linux")]
#[test]
fn every_module_case_matches_the_reference() {
    let Some(python) = python::reference_python() else {
        return;
    };
    assert!(
        compare::identity().uid != 0,
        "run as root: the recording's literal `root` would read as the running account's own; run \
         as an ordinary account with passwordless `sudo -n`"
    );
    // Before the machine checks: another run holding the lock is between `apt-install-absent` and
    // its cleanup, with `hello` installed, and the check would read that as this machine's state.
    let _lock = native_lock();
    let mut missing: Vec<String> = [
        ("sudo -n true", "passwordless `sudo -n`"),
        ("test -d /run/systemd/system", "running systemd"),
        ("command -v apt-get", "apt"),
        (
            "systemctl is-active --quiet cron && systemctl is-enabled --quiet cron",
            "cron active and enabled",
        ),
        (
            "test \"$(dpkg-query -W -f='${db:Status-Status}' hello 2>/dev/null)\" != installed",
            "`hello` not installed (the play installs and removes it)",
        ),
    ]
    .into_iter()
    .filter(|(probe, _)| !succeeds(probe))
    .map(|(_, what)| what.to_string())
    .collect();
    let mismatched = collections::collection_mismatches(&python);
    if !mismatched.is_empty() {
        missing.push(format!(
            "the pinned collections ({})",
            mismatched.join("; ")
        ));
    }
    if !missing.is_empty() {
        let why = format!(
            "the native cases were recorded with `become`, systemd, apt and cron, and this \
             machine lacks: {}",
            missing.join(", ")
        );
        assert!(
            std::env::var_os("VOLANT_PYTHON").is_none(),
            "VOLANT_PYTHON cannot reproduce the recording: {why}"
        );
        eprintln!("skipped: {why}.");
        return;
    }

    let index = module_file("index.json");
    let index = index.as_object().expect("index.json is a mapping");
    let mut recorded = module_file("play.json");
    replace_in_strings(&mut recorded, "<golden-tmp>", NATIVE_DIR);
    replace_in_strings(&mut recorded, "<golden-action-src>", ACTION_SRC);
    let mut play = recorded.clone();
    // Volant refuses the `connection` play keyword; the inventory `run_recorded_play` writes
    // says `local` instead.
    play[0]
        .as_object_mut()
        .expect("the recorded play is a mapping")
        .remove("connection");
    // After each case, its registered value, which keeps `failed` where the `fatal:` line does
    // not. The read-backs that follow use their own names and leave `last` alone.
    let block = play[0]["tasks"][0]["block"]
        .as_array_mut()
        .expect("the recorded play is one block");
    *block = std::mem::take(block)
        .into_iter()
        .flat_map(|task| {
            let registered = task["name"]
                .as_str()
                .filter(|name| index.contains_key(*name))
                .map(|name| {
                    serde_json::json!({"name": format!("registered-{name}"), "debug": {"var": "last"}})
                });
            std::iter::once(task).chain(registered)
        })
        .collect();

    // Safety: nextest runs each test in its own process, and nothing here creates files
    // concurrently.
    unsafe { libc::umask(0o022) };
    native_fixture();
    // Outside NATIVE_DIR, as in the generator: `stat-dir` reports the directory's link count. Beside
    // it under `/var/tmp` all the same, because the agent stages `copy`'s source under this
    // directory: the reference staged its own under `~/.ansible/tmp`, on the file system of
    // NATIVE_DIR, and renamed it into place, where a staging directory on a tmpfs `/tmp` would
    // cross a mount and send the native `copy` back to Python.
    let work = std::path::PathBuf::from(format!(
        "/var/tmp/volant-golden-native-work-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("the work directory is writable");
    // Read by both runs instead of the account's own `~/.ansible.cfg`, whose `[volant]
    // native_modules = false` or `become_user` would change one side's answer.
    std::fs::write(work.join("ansible.cfg"), "").expect("the empty config is written");
    std::fs::write(
        work.join("natives.yml"),
        serde_json::to_string_pretty(&play).expect("the play serialises"),
    )
    .expect("the play is written");
    let profile = work.join("profile.jsonl");
    // The play's collection cases need the pinned collections wherever the job installed them.
    let mut env = collections_path_env();
    env.push(("VOLANT_PROFILE_JSON", profile.display().to_string()));
    env.push((
        "ANSIBLE_CONFIG",
        work.join("ansible.cfg").display().to_string(),
    ));
    let out = run_recorded_play(
        &work,
        "natives.yml",
        &python,
        &["-vvv", "--facts", "native"],
        &env,
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let results = results_by_task(&stdout);
    let failed = failed_tasks(&stdout);
    let mut failures = Vec::new();
    // Every failing case and every cleanup carries `ignore_errors`, so a sound run exits 0 and
    // runs its whole `always`: a native that answers its case and then breaks the agent, or ends
    // the run, is caught here and by no case.
    if !out.status.success() {
        failures.push(format!(
            "volant exited {:?}, where every failure is ignored",
            out.status.code()
        ));
    }
    // The recorded play's cleanups, not those of the play Volant was handed.
    for task in recorded[0]["tasks"][0]["always"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let name = task["name"].as_str().unwrap_or_default();
        if !results.contains_key(name) {
            failures.push(format!("the cleanup {name} did not run"));
        } else if failed.contains(name) {
            // Ignored, so it does not end the run; but a `hello` left installed refuses every
            // later run, and a unit or account left behind changes the next one's answers.
            failures.push(format!("the cleanup {name} failed"));
        }
    }
    // The reference replays the whole recorded play from the same fixture, so each live case
    // meets the state the cases before it left.
    native_fixture();
    let live = reference_results(&python, &work, &recorded);
    for task in recorded[0]["tasks"][0]["always"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let name = task["name"].as_str().unwrap_or_default();
        if live
            .get(name)
            .is_none_or(|outcome| outcome["failed"] == true)
        {
            failures.push(format!(
                "the reference's cleanup {name} did not run, or failed"
            ));
        }
    }
    let staged_root = format!("{}/tmp/", work.display());
    let masks = NativeMasks::new(staged_root.clone());
    let profile = native_profile(&profile, "localhost");
    for native in profile.natives.iter().flatten() {
        if native != "volant_echo"
            && !index
                .values()
                .any(|spec| spec["module"] == *native && spec["expect"] == "native")
        {
            failures.push(format!(
                "the agent declares a {native} native, and no indexed case expects it to answer"
            ));
        }
    }
    let id = compare::identity();
    for (case, spec) in index {
        // The modules no native answers: the registered value against the recording, by the
        // index entry's `shape` rules. Their path is not held: a native may answer `copy`'s module.
        if spec["compare"] == "shape" {
            let recording = module_file(&format!("{case}.json"));
            if recording["action"] != spec["module"] {
                failures.push(format!(
                    "case {case}: recorded from a task that ran {}",
                    recording["action"]
                ));
            }
            match shape_result(case, &results) {
                Ok(mut ours) => {
                    redact_staged(&mut ours, &staged_root);
                    match (recording.as_object(), ours.as_object()) {
                        (Some(want), Some(got)) => {
                            compare_shape(case, spec, want, got, &id, &mut failures);
                        }
                        _ => failures
                            .push(format!("case {case}: reference {recording}, ours {ours}")),
                    }
                }
                Err(why) => failures.push(format!("case {case}: {why}")),
            }
            continue;
        }
        let mut ours = match native_result(case, spec, &results) {
            Ok(ours) => ours,
            Err(why) => {
                failures.push(format!("case {case}: {why}"));
                continue;
            }
        };
        masks.apply(&mut ours);
        let reference = match spec["compare"].as_str() {
            Some("live") => {
                let Some(mut reference) = live.get(case).cloned() else {
                    failures.push(format!("case {case}: the reference gave no live result"));
                    continue;
                };
                if let Some(read) = live.get(&format!("after-unit-{case}"))
                    && let Some(map) = reference.as_object_mut()
                {
                    match unit_after(read) {
                        Ok(after) => {
                            map.insert("_after".into(), after);
                        }
                        Err(why) => failures.push(format!("case {case}: the reference's {why}")),
                    }
                }
                masks.apply(&mut reference);
                same_names(case, &reference, &ours, &mut failures);
                keep_live(&mut reference);
                keep_live(&mut ours);
                live_branches(case, &reference, &mut failures);
                reference
            }
            Some("exact" | "keys") => module_file(&format!("{case}.json")),
            other => panic!("{case}: compare {other:?} is none of exact, keys, live"),
        };
        match (reference.as_object(), ours.as_object()) {
            (Some(want), Some(got)) => compare_native(case, spec, "", want, got, &mut failures),
            _ => failures.push(format!("case {case}: reference {reference}, ours {ours}")),
        }
        check_native_path(case, spec, &profile, &mut failures);
    }
    let _ = std::fs::remove_dir_all(NATIVE_DIR);
    let _ = std::fs::remove_dir_all(&work);
    assert!(
        failures.is_empty(),
        "{} difference(s) across {} module case(s):\n{}\n--- volant said\n{stdout}\n--- stderr\n{}",
        failures.len(),
        index.len(),
        failures.join("\n"),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The generator's fixture under NATIVE_DIR, mode for mode: `stat-*` report these, and
/// `copy-module-remote-src` creates its file under the umask the caller set.
#[cfg(target_os = "linux")]
fn native_fixture() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = std::path::Path::new(NATIVE_DIR);
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("the recorded directory is writable");
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
        .expect("the directory's mode is set");
    std::fs::write(dir.join("f.txt"), "hello\n").expect("the fixture is written");
    std::fs::set_permissions(dir.join("f.txt"), std::fs::Permissions::from_mode(0o644))
        .expect("the fixture's mode is set");
    std::os::unix::fs::symlink("f.txt", dir.join("l")).expect("the link is made");
}

/// The reference's kept units, once `keep_live` has run, must hold a SysV entry and a template:
/// without them the comparison would not see those two branches, and would pass on a machine
/// that lacks them.
#[cfg(target_os = "linux")]
fn live_branches(case: &str, reference: &Value, failures: &mut Vec<String>) {
    let Some(services) = reference["ansible_facts"]["services"].as_object() else {
        return;
    };
    if services
        .get("cron")
        .is_none_or(|cron| cron["source"] != "sysv")
    {
        failures.push(format!(
            "case {case}: the reference lists no SysV cron, so the SysV branch goes unchecked"
        ));
    }
    if !services.keys().any(|name| name.ends_with("@.service")) {
        failures.push(format!(
            "case {case}: the reference lists no template unit, so the unit file branch goes \
             unchecked"
        ));
    }
}

/// `volant_echo` as a Python module ansible-core builds under `ansible.modules`, for
/// `a_native_answer_is_held_to_the_path_its_index_names`.
#[cfg(target_os = "linux")]
const ECHO_MODULE: &str = r#"from ansible.module_utils.basic import AnsibleModule


def main():
    module = AnsibleModule(argument_spec={"x": {"type": "int"}, "fallback": {"type": "str"}})
    module.exit_json(echo=module.params)


if __name__ == "__main__":
    main()
"#;

/// `volant_echo`, a native only a `test-natives` agent has: green under an index naming its path,
/// red on the path alone under one that does not.
#[cfg(target_os = "linux")]
#[test]
fn a_native_answer_is_held_to_the_path_its_index_names() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let Some(agents) = std::env::var_os("VOLANT_TEST_NATIVES_AGENT_DIR") else {
        let why = "VOLANT_TEST_NATIVES_AGENT_DIR names no agent built with the test-natives \
                   feature (`just ssh-test` builds one)";
        assert!(std::env::var_os("VOLANT_PYTHON").is_none(), "{why}");
        eprintln!("skipped: {why}.");
        return;
    };
    let work = std::env::temp_dir().join(format!("volant-golden-echo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let out = std::process::Command::new(&python)
        .args([
            "-c",
            "import ansible, os; print(os.path.dirname(os.path.realpath(ansible.__file__)))",
        ])
        .output()
        .expect("the controller python runs");
    let real = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let overlay = work.join("overlay");
    let modules = overlay.join("ansible/modules");
    std::fs::create_dir_all(&modules).expect("the overlay is writable");
    let link_all = |from: &std::path::Path, to: &std::path::Path, but: &str| {
        for entry in std::fs::read_dir(from).expect("ansible-core's directory lists") {
            let entry = entry.expect("ansible-core's directory lists");
            if entry.file_name() != but {
                std::os::unix::fs::symlink(entry.path(), to.join(entry.file_name()))
                    .expect("the overlay links");
            }
        }
    };
    link_all(&real, &overlay.join("ansible"), "modules");
    link_all(&real.join("modules"), &modules, "__init__.py");
    // A copy, not a link: the controller tells ansible-core's own module by the real directory of
    // `ansible.modules`, which has to be this one.
    std::fs::copy(
        real.join("modules/__init__.py"),
        modules.join("__init__.py"),
    )
    .expect("the overlay's package is made");
    std::fs::write(modules.join("volant_echo.py"), ECHO_MODULE).expect("the module is written");
    // Volant knows ansible-core's own module names by heart and refuses any other bare name, so
    // the play names a collection that redirects to the builtin, as `ansible.posix` redirects
    // some of its names to ansible-core's modules.
    let meta = work.join("collections/ansible_collections/acme/echo/meta");
    std::fs::create_dir_all(&meta).expect("the collection is writable");
    std::fs::write(
        meta.join("runtime.yml"),
        "plugin_routing:\n  modules:\n    volant_echo:\n      redirect: ansible.builtin.volant_echo\n",
    )
    .expect("the routing is written");
    std::fs::write(
        work.join("echo.yml"),
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n\
         \x20   - name: echo-native\n      acme.echo.volant_echo: {x: 1}\n      register: last\n\
         \x20   - name: registered-echo-native\n      debug: {var: last}\n\
         \x20   - name: echo-fallback\n      acme.echo.volant_echo: {x: 1, fallback: asked}\n      register: last\n\
         \x20   - name: registered-echo-fallback\n      debug: {var: last}\n",
    )
    .expect("the play is written");
    let profile = work.join("profile.jsonl");
    let out = run_recorded_play(
        &work,
        "echo.yml",
        &python,
        &["-vvv"],
        &[
            ("PYTHONPATH", overlay.display().to_string()),
            (
                "ANSIBLE_COLLECTIONS_PATH",
                work.join("collections").display().to_string(),
            ),
            ("VOLANT_AGENT_DIR", agents.to_string_lossy().into_owned()),
            ("XDG_CACHE_HOME", work.join("cache").display().to_string()),
            ("VOLANT_PROFILE_JSON", profile.display().to_string()),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let results = results_by_task(&stdout);
    let profile = native_profile(&profile, "localhost");
    assert!(
        profile
            .natives
            .iter()
            .flatten()
            .any(|name| name == "volant_echo"),
        "the agent in {} declares no volant_echo: {:?}\n{stdout}\n--- stderr\n{}",
        agents.to_string_lossy(),
        profile.natives,
        String::from_utf8_lossy(&out.stderr)
    );
    // `volant_echo` has no reference module: its recording cannot carry the discovered-interpreter
    // fact and warning the first module task on a host gets, and this test is about the path.
    let entry = |expect: &str| serde_json::json!({"module": "volant_echo", "expect": expect, "volatile": [], "patterns": {}, "unordered": []});
    let recorded = serde_json::json!({
        "echo-native": {"changed": false, "echo": {"x": 1}},
        "echo-fallback": {
            "changed": false,
            "echo": {"x": 1, "fallback": "asked"},
            "invocation": {"module_args": {"x": 1, "fallback": "asked"}},
        },
    });
    let judge = |index: &Value| {
        let mut failures = Vec::new();
        for (case, spec) in index.as_object().expect("an index is a mapping") {
            match native_result(case, spec, &results) {
                Ok(ours) => {
                    let mut ours = ours.as_object().expect("a result is a mapping").clone();
                    ours.remove("ansible_facts");
                    ours.remove("warnings");
                    compare_native(
                        case,
                        spec,
                        "",
                        recorded[case]
                            .as_object()
                            .expect("a recording is a mapping"),
                        &ours,
                        &mut failures,
                    );
                }
                Err(why) => failures.push(format!("case {case}: {why}")),
            }
            check_native_path(case, spec, &profile, &mut failures);
        }
        failures
    };
    let right = judge(&serde_json::json!({
        "echo-native": entry("native"),
        "echo-fallback": entry("fallback"),
    }));
    let wrong = judge(&serde_json::json!({
        "echo-native": entry("fallback"),
        "echo-fallback": entry("fallback"),
    }));
    let _ = std::fs::remove_dir_all(&work);
    assert!(
        right.is_empty(),
        "{}\n--- volant said\n{stdout}\n--- stderr\n{}",
        right.join("\n"),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        wrong,
        vec!["case echo-native: path native, index says fallback".to_string()]
    );
}

/// Filters, tests and lookups run in the Python helper answer with ansible-core's own result
/// and sentences. The terms of a lookup stay data: a template in a variable comes back as is.
#[rstest]
#[case::filter("filter", "ansible.builtin.b64encode", serde_json::json!(["a"]), serde_json::json!({}), Ok(serde_json::json!("YQ==")))]
#[case::filter_keyword("filter", "join", serde_json::json!([["a", "b"]]), serde_json::json!({"d": "-"}), Ok(serde_json::json!("a-b")))]
#[case::a_test("test", "divisibleby", serde_json::json!([4, 2]), serde_json::json!({}), Ok(serde_json::json!(true)))]
#[case::lookup("lookup", "vars", serde_json::json!(["x"]), serde_json::json!({}), Ok(serde_json::json!("{{ 1 + 1 }}")))]
#[case::plugin_error("filter", "join", serde_json::json!([["a"]]), serde_json::json!({"sep": "-"}), Err(("Plugin", "The filter plugin 'ansible.builtin.join' failed:")))]
#[case::not_a_number("filter", "float", serde_json::json!(["nan"]), serde_json::json!({}), Err(("Plugin", "JSON cannot carry")))]
#[case::term_stays_data("lookup", "vars", serde_json::json!(["{{ y }}"]), serde_json::json!({}), Err(("Plugin", "{{ y }}")))]
#[case::subprocess_cannot_read_requests("lookup", "pipe", serde_json::json!(["cat"]), serde_json::json!({}), Ok(serde_json::json!("")))]
#[case::unknown_filter("filter", "nope", serde_json::json!([1]), serde_json::json!({}), Err(("NotFound", "No filter named 'nope'.")))]
#[case::unknown_test("test", "nope", serde_json::json!([1]), serde_json::json!({}), Err(("NotFound", "No test named 'nope'.")))]
#[case::unknown_lookup("lookup", "nope", serde_json::json!([1]), serde_json::json!({}), Err(("NotFound", "The lookup plugin 'nope' was not found.")))]
fn the_bridge_runs_what_only_ansible_core_can(
    #[case] kind: &str,
    #[case] name: &str,
    #[case] args: Value,
    #[case] kwargs: Value,
    #[case] ends: Result<Value, (&'static str, &'static str)>,
) {
    use volant::bridge::{Bridge, BridgeError, PluginKind};

    let Some(python) = python::reference_python() else {
        return;
    };
    let kind = match kind {
        "filter" => PluginKind::Filter,
        "test" => PluginKind::Test,
        _ => PluginKind::Lookup,
    };
    let vars = serde_json::json!({"x": "{{ 1 + 1 }}", "y": "Y"});
    let bridge = Bridge::under(python, Vec::new());
    let got = bridge.call(
        kind,
        name,
        args.as_array().unwrap(),
        kwargs.as_object().unwrap(),
        vars.as_object(),
    );
    // Whatever the call ended with, the next one still gets its answer.
    let next = bridge.call(
        PluginKind::Filter,
        "b64encode",
        &[serde_json::json!("a")],
        &Map::new(),
        None,
    );
    assert_eq!(next, Ok(serde_json::json!("YQ==")));
    match (got, ends) {
        (Ok(got), Ok(want)) => assert_eq!(got, want),
        (Err(BridgeError::Plugin(text)), Err(("Plugin", part)))
        | (Err(BridgeError::NotFound(text)), Err(("NotFound", part))) => {
            assert!(text.contains(part), "{text}");
        }
        (got, want) => panic!("{got:?} is not {want:?}"),
    }
}

/// Every `bridge` row of `cases.yml` rendered with the Python helper behind the templar: the
/// value or the failure ansible-core recorded. The controller files are laid out first.
#[test]
fn every_bridge_case_matches_the_reference() {
    use volant::bridge::Bridge;
    use volant::template::{Templar, Vars};

    // Panics under `VOLANT_PYTHON` when that interpreter cannot reproduce the recording.
    let Some(python) = python::reference_python() else {
        return;
    };
    let fs: Value = serde_json::from_str(include_str!("golden/bridge_fs.json")).unwrap();
    let dir = std::path::PathBuf::from(fs["dir"].as_str().unwrap());
    // The directory is the same for every run on this machine: one run at a time lays it out,
    // and it goes away when the test ends, a panic included.
    struct Removed(std::path::PathBuf);
    impl Drop for Removed {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _lock = native_lock();
    let _removed = Removed(dir.clone());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for name in fs["dirs"].as_array().unwrap() {
        std::fs::create_dir(dir.join(name.as_str().unwrap())).unwrap();
    }
    for (name, text) in fs["files"].as_object().unwrap() {
        std::fs::write(dir.join(name), text.as_str().unwrap()).unwrap();
    }
    for (name, target) in fs["links"].as_object().unwrap() {
        std::os::unix::fs::symlink(target.as_str().unwrap(), dir.join(name)).unwrap();
    }
    // Safety: nextest runs each test in its own process; nothing else reads the environment here.
    unsafe { std::env::set_var("VOLANT_GOLDEN_ENV", "golden-env-value") };
    let templar = Templar::with_bridge(dir.clone(), Some(Bridge::under(python, Vec::new())));
    let entries: Vec<Value> =
        serde_json::from_str(include_str!("golden/expected.json")).expect("expected.json parses");
    let mut failures = Vec::new();
    let mut ran = 0;
    for entry in entries.iter().filter(|e| e["case"]["bridge"] == true) {
        ran += 1;
        let case = &entry["case"];
        let vars: Map<String, Value> = case
            .get("vars")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let vars = Vars {
            map: &vars,
            hostvars: None,
            shared: None,
            facts: None,
            untrusted: None,
            untrusted_hosts: None,
        };
        let text = case["template"].as_str().expect("template is a string");
        let ours = templar.render(text, vars).map_err(|e| e.to_string());
        // A failure holds the reference's `The <kind> plugin '<name>' failed` sentence.
        let right = match (&ours, entry.get("error"), entry.get("result")) {
            (Err(text), Some(want), _) => {
                let want = want.as_str().unwrap_or_default();
                let start = want.find("The ").unwrap_or(0);
                let sentence = want[start..].split(" failed").next().unwrap_or_default();
                text.contains(&format!("{sentence} failed"))
            }
            (Ok(v), None, Some(want)) => same(v, want),
            _ => false,
        };
        if !right {
            failures.push(format!(
                "{case}
  reference: {}
  ours: {ours:?}",
                serde_json::to_string(entry).unwrap()
            ));
        }
    }
    assert!(ran >= 80, "{ran} bridge cases: cases.yml lost some");
    assert!(
        failures.is_empty(),
        "{} case(s) differ from ansible-core:
{}",
        failures.len(),
        failures.join(
            "
"
        )
    );
}

/// A local action plugin that asks the controller for each thing a plugin may ask of the host,
/// once, and one that reads a connection member the stand-in connection does not have.
const ACTION_PLUGINS: [(&str, &str); 4] = [
    (
        "every_ask.py",
        r#"from ansible.plugins.action import ActionBase


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        result = super().run(tmp, task_vars)
        made = self._make_tmp_path()
        command = self._low_level_execute_command("echo hi", sudoable=False)
        self._connection.put_file("/local/in", made + "/in")
        self._connection.fetch_file(made + "/in", "/local/out")
        module = self._execute_module(module_name="ansible.builtin.ping", module_args={"data": "x"}, task_vars=task_vars)
        other = task_vars["hostvars"]["other"]["v"]
        self._remove_tmp_path(made)
        result.update(rc=command["rc"], stdout=command["stdout"], module=module, other=other, args=self._task.args)
        return result
"#,
    ),
    (
        "refused.py",
        r#"from ansible.plugins.action import ActionBase


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        return {"path": self._connection.socket_path}
"#,
    ),
    (
        "unreachable.py",
        r#"from ansible.plugins.action import ActionBase


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        return self._low_level_execute_command("true", sudoable=False)
"#,
    ),
    (
        "skipped.py",
        r#"from ansible.errors import AnsibleActionSkip
from ansible.plugins.action import ActionBase


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        raise AnsibleActionSkip("not here")
"#,
    ),
];

/// Action plugins run in the Python helper as ansible-core runs them: each question reaches the
/// controller once, the arguments stay data, and the result or failure is ansible-core's.
#[rstest]
#[case::collection_plugin("ansible.utils.update_fact", serde_json::json!({"updates": [{"path": "a.b", "value": 2}]}), &[], Ok(serde_json::json!({"changed": true, "a": {"b": 2}})))]
#[case::every_ask("every_ask", serde_json::json!({"k": "{{ a }}"}), &[
    "Tmp(Make { remote_user: None })",
    "Command { cmd: \"/bin/sh -c 'echo hi && sleep 0'\", stdin: None, sudoable: false }",
    "PutFile { local: \"/local/in\", remote: \"/tmp/ansible-tmp-1/in\", mode: None }",
    "FetchFile { remote: \"/tmp/ansible-tmp-1/in\", local: \"/local/out\" }",
    "Module { name: \"ansible.builtin.ping\", args: Object {\"data\": String(\"x\"), \"_ansible_module_name\": String(\"ansible.builtin.ping\")}, files: [] }",
    "HostVar { host: \"other\" }",
    "Tmp(Remove { path: \"/tmp/ansible-tmp-1\" })",
], Ok(serde_json::json!({"rc": 0, "stdout": "hi\n", "module": {"ping": "x"}, "other": 7, "args": {"k": "{{ a }}"}})))]
#[case::member_not_supported("refused", serde_json::json!({}), &[], Ok(serde_json::json!({"failed": true, "msg": "Task failed: Connection.socket_path is not supported yet"})))]
#[case::link_failure_is_unreachable("unreachable", serde_json::json!({}), &["Command { cmd: \"/bin/sh -c 'true && sleep 0'\", stdin: None, sudoable: false }"], Ok(serde_json::json!({"unreachable": true, "msg": "Task failed: the host is gone", "changed": false})))]
#[case::skip("skipped", serde_json::json!({}), &[], Ok(serde_json::json!({"skipped": true, "msg": "not here", "changed": false})))]
#[case::unknown_plugin("my.col.nosuch", serde_json::json!({}), &[], Err("the handler 'my.col.nosuch' was not found"))]
fn the_bridge_runs_action_plugins(
    #[case] name: &str,
    #[case] args: Value,
    #[case] want_asks: &[&str],
    #[case] ends: Result<Value, &str>,
) {
    use volant::bridge::{Ask, Bridge, BridgeError, Reply, TmpOp};

    let Some(python) = python::reference_python() else {
        return;
    };
    let work = std::env::temp_dir().join(format!("volant-action-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("action_plugins")).unwrap();
    for (file, text) in ACTION_PLUGINS {
        std::fs::write(work.join("action_plugins").join(file), text).unwrap();
    }
    let bridge = Bridge::under(python, vec![work.clone()]);
    let mut asked = Vec::new();
    let deadline = std::time::Instant::now() + DEADLINE;
    let got = bridge.run_action(
        name,
        &serde_json::json!({ "args": args }),
        &serde_json::json!({"a": {"b": 1}, "groups": {"all": ["h", "other"]}}),
        &serde_json::json!({"connection": "ssh", "remote_addr": "h", "remote_user": "u"}),
        deadline,
        &mut |ask| {
            let reply = match &ask {
                Ask::Tmp(TmpOp::Make { .. }) => serde_json::json!("/tmp/ansible-tmp-1"),
                Ask::Command { .. } => serde_json::json!({"rc": 0, "stdout": "hi\n", "stderr": ""}),
                Ask::Module { .. } => serde_json::json!({"ping": "x"}),
                Ask::HostVar { .. } => serde_json::json!({"v": 7}),
                _ => Value::Null,
            };
            asked.push(format!("{ask:?}"));
            if name == "unreachable" {
                return Reply::Error("the host is gone".into());
            }
            Reply::Value(reply)
        },
    );
    let _ = std::fs::remove_dir_all(&work);
    assert_eq!(asked, want_asks);
    match (got, ends) {
        (Ok(got), Ok(want)) => {
            for (key, value) in want.as_object().unwrap() {
                assert_eq!(got.get(key), Some(value), "{key} in {got}");
            }
        }
        (Err(BridgeError::NotFound(text)), Err(want)) => assert_eq!(text, want),
        (got, want) => panic!("{got:?} is not {want:?}"),
    }
}
