// SPDX-License-Identifier: GPL-3.0-or-later
//! What the native module goldens (`golden.rs`) and the native end-to-end tests (`ssh_e2e.rs`)
//! share: reading a run's results and profile, comparing two results key by key under an index
//! entry, holding a case to the path the index names, and the lock over the machine state the
//! native cases change. Each includer declares `setup_exits` beside this module.
#[cfg(target_os = "linux")]
use serde_json::Map;
use serde_json::Value;

/// Python's `2 == 2.0` is true; serde_json's is not. Integral floats compare equal to integers.
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => (x - y).abs() < f64::EPSILON,
            _ => x == y,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

/// Compared only when both sides carry the key.
///
/// `_ansible_no_log` comes from the same executor bookkeeping as `invocation`. It is `false` in
/// every fixture and can only be anything else under `no_log: true`, which no recorded task uses,
/// so a by-value comparison could go red in exactly one situation: Volant declining to emit an
/// ansible-core internal for a task that never asked for it. The fix that would suggest itself is
/// hard-coding `false` into Volant's output to please the fixture.
#[cfg(target_os = "linux")]
pub const IF_BOTH: &[&str] = &["_ansible_no_log"];

/// `ok:`/`changed:`/`fatal:` lines at `-v`, keyed by the task banner they arrived under.
#[cfg(target_os = "linux")]
pub fn results_by_task(stdout: &str) -> Map<String, Value> {
    let mut results = Map::new();
    let mut task = String::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("TASK [")
            && let Some(end) = rest.find(']')
        {
            task = rest[..end].to_string();
        } else if ["ok: [", "changed: [", "fatal: ["]
            .iter()
            .any(|p| line.starts_with(p))
            && let Some((_, json)) = line.split_once(" => ")
            && let Ok(value) = serde_json::from_str(json)
        {
            results.insert(task.clone(), value);
        }
    }
    results
}

/// The tasks of a run that printed a `fatal:` line, `...ignoring` or not, by the banner they came
/// under.
#[cfg(target_os = "linux")]
pub fn failed_tasks(stdout: &str) -> std::collections::BTreeSet<String> {
    let mut failed = std::collections::BTreeSet::new();
    let mut task = "";
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("TASK [")
            && let Some(end) = rest.find(']')
        {
            task = &rest[..end];
        } else if line.starts_with("fatal: [") {
            failed.insert(task.to_string());
        }
    }
    failed
}

/// `NATIVE_LOCK` in `generate.py`, taken and held until the returned file is dropped: the
/// fixture, the accounts, the unit and the package are the machine's, so one run at a time, this
/// test's or the generator's. Under the account's own home, where no other account can create the
/// file first and keep it from being opened; the runs that share the state are this account's.
#[cfg(target_os = "linux")]
pub fn native_lock() -> std::fs::File {
    use std::os::fd::AsRawFd as _;
    let home = std::env::var_os("HOME").expect("HOME is set");
    let path = std::path::Path::new(&home).join(".cache/volant-golden-native.lock");
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("~/.cache is writable");
    let lock = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap_or_else(|e| panic!("{} opens: {e}", path.display()));
    // Safety: a valid descriptor, open for as long as the lock is held.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!(
            "waiting for {}: another native golden run holds it",
            path.display()
        );
        // Safety: as above.
        let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        assert_eq!(locked, 0, "{} is lockable", path.display());
    }
    lock
}

#[cfg(target_os = "linux")]
pub fn replace_in_strings(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(s) if s.contains(from) => *s = s.replace(from, to),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|v| replace_in_strings(v, from, to)),
        Value::Object(map) => map
            .values_mut()
            .for_each(|v| replace_in_strings(v, from, to)),
        _ => {}
    }
}

/// An `unordered` value's words, split on single spaces as `sorted(value.split(" "))` does.
#[cfg(target_os = "linux")]
pub fn words(value: &Value) -> Option<Vec<&str>> {
    let mut words: Vec<&str> = value.as_str()?.split(' ').collect();
    words.sort_unstable();
    Some(words)
}

/// One case's result against the reference's, key by key, `invocation` included, loosened only
/// where the case's index entry says: a `volatile` path is on both sides and its value is not
/// compared, a `patterns` path matches its regex, an `unordered` path is compared as the sorted
/// list of its space-separated words.
#[cfg(target_os = "linux")]
pub fn compare_native(
    case: &str,
    spec: &Value,
    prefix: &str,
    want: &Map<String, Value>,
    got: &Map<String, Value>,
    failures: &mut Vec<String>,
) {
    let listed = |field: &str, path: &str| {
        spec[field]
            .as_array()
            .is_some_and(|paths| paths.iter().any(|p| p == path))
    };
    let shown = |v: Option<&Value>| v.map_or_else(|| "absent".to_string(), Value::to_string);
    let mut keys: Vec<&String> = want.keys().chain(got.keys()).collect();
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        let (reference, ours) = (want.get(key), got.get(key));
        // `action` is the JSON callback's copy of the task's module name, which no module
        // returns (see `DROPPED` in golden.rs). An `_ansible_` key only the recording has is executor
        // bookkeeping the JSON callback shows and the reference strips from both a registered
        // value and a `-vvv` line (`strip_internal_keys`): `_ansible_no_log`, and `setup`'s
        // `_ansible_verbose_override`.
        if path == "action"
            || (IF_BOTH.contains(&path.as_str()) && (reference.is_none() || ours.is_none()))
            || (key.starts_with("_ansible_") && prefix.is_empty() && ours.is_none())
        {
            continue;
        }
        let (Some(reference), Some(ours)) = (reference, ours) else {
            failures.push(format!(
                "case {case}: key {path}: reference {}, ours {}",
                shown(reference),
                shown(ours)
            ));
            continue;
        };
        if listed("volatile", &path) {
            continue;
        }
        if let Some(pattern) = spec["patterns"].get(&path).and_then(Value::as_str) {
            let text = ours
                .as_str()
                .map_or_else(|| ours.to_string(), str::to_string);
            if !regex::Regex::new(pattern)
                .expect("an index pattern compiles")
                .is_match(&text)
            {
                failures.push(format!(
                    "case {case}: key {path}: ours {ours} does not match {pattern}"
                ));
            }
            continue;
        }
        if listed("unordered", &path) {
            if words(reference).is_none() || words(reference) != words(ours) {
                failures.push(format!(
                    "case {case}: key {path}: reference {reference}, ours {ours}, in any order"
                ));
            }
            continue;
        }
        match (reference, ours) {
            (Value::Object(want), Value::Object(got)) => {
                compare_native(case, spec, &path, want, got, failures);
            }
            _ if same(reference, ours) => {}
            _ => failures.push(format!(
                "case {case}: key {path}: reference {reference}, ours {ours}"
            )),
        }
    }
}

/// What `VOLANT_PROFILE_JSON` says of a run on `host`: the natives its agent declared, and
/// one line per task the agent ran.
#[cfg(target_os = "linux")]
pub struct NativeProfile {
    pub natives: Option<Vec<String>>,
    pub tasks: Vec<Value>,
}

#[cfg(target_os = "linux")]
pub fn native_profile(path: &std::path::Path, host: &str) -> NativeProfile {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("VOLANT_PROFILE_JSON left no {}: {e}", path.display()));
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a profile line is JSON"))
        .collect();
    let natives = lines
        .first()
        .and_then(|first| first["natives"][host].as_array())
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });
    let tasks = lines
        .into_iter()
        .filter(|line| line.get("task").is_some() && line["host"] == host)
        .collect();
    NativeProfile { natives, tasks }
}

/// Whether a `package_facts` hand-back is the host's apt sources naming a scheme other than
/// http(s), which the native does not read (`the source <uri> is not http or https`), rather
/// than the native failing on a host it should answer.
#[cfg(target_os = "linux")]
pub fn package_facts_host_exit(reason: &str) -> bool {
    reason.starts_with("the source ")
        && reason.ends_with(" is not http or https")
        && !reason.starts_with("the source http://")
        && !reason.starts_with("the source https://")
}

/// The path the case's own module took, against the one it must take: the index's when the agent
/// declared a native for the module, `python` otherwise. A plugin's sub-tasks (`copy`'s `stat`)
/// have lines of their own under the same task and are not the case's.
#[cfg(target_os = "linux")]
pub fn check_native_path(
    case: &str,
    spec: &Value,
    profile: &NativeProfile,
    failures: &mut Vec<String>,
) {
    let module = spec["module"].as_str().unwrap_or_default();
    let Some(natives) = &profile.natives else {
        failures.push(format!(
            "case {case}: the profile names no natives for the host, so no path can be required"
        ));
        return;
    };
    let lines: Vec<&Value> = profile
        .tasks
        .iter()
        .filter(|line| line["task"] == case)
        .collect();
    let own: Vec<&Value> = lines
        .iter()
        .copied()
        .filter(|line| {
            line["module"]
                .as_str()
                .is_some_and(|m| m.rsplit('.').next() == Some(module))
        })
        .collect();
    if own.is_empty() {
        let modules: Vec<&Value> = lines.iter().map(|line| &line["module"]).collect();
        failures.push(format!(
            "case {case}: the profile has no line for {module}, only {modules:?}"
        ));
        return;
    }
    let native = natives.iter().any(|name| name == module);
    let want = if native {
        spec["expect"].as_str().unwrap_or_default()
    } else {
        "python"
    };
    for line in own {
        let got = line["path"].as_str().unwrap_or("unreported");
        // The reason a native gave for handing back, which is what a red here is about.
        let why = line["reason"]
            .as_str()
            .map(|reason| format!(" ({reason})"))
            .unwrap_or_default();
        // A host outside the native setup's subset (a runner with `/usr/bin/rpm`, measured) hands
        // `setup` back wherever it runs: said, and not held against the native. The result is
        // then the Python module's, compared all the same.
        if native
            && module == "setup"
            && got == "fallback"
            && line["reason"]
                .as_str()
                .is_some_and(super::setup_exits::is_host_exit)
        {
            eprintln!(
                "case {case}: this host is outside the native setup's subset{why}: compared on \
                 the Python answer"
            );
            continue;
        }
        // A host whose apt sources use a scheme the native does not name lists for (GitHub's
        // runner reads `mirror+file:`, measured) hands `package_facts` back: said, and compared
        // on the Python answer. Only that reason: on a host with http(s) sources alone, any
        // hand-back is still red.
        if native
            && module == "package_facts"
            && got == "fallback"
            && line["reason"].as_str().is_some_and(package_facts_host_exit)
        {
            eprintln!(
                "case {case}: this host's apt sources are outside the native package_facts{why}: \
                 compared on the Python answer"
            );
            continue;
        }
        if got != want {
            failures.push(if native {
                format!("case {case}: path {got}{why}, index says {want}")
            } else {
                format!(
                    "case {case}: path {got}{why}, and {module} is no native of this agent: python"
                )
            });
        }
    }
}

/// The names of every package and every unit, compared as sets before `keep_live` narrows the
/// values: a `service_facts` that lists two units, or a `package_facts` that lists `bash`, would
/// otherwise pass.
#[cfg(target_os = "linux")]
pub fn same_names(case: &str, reference: &Value, ours: &Value, failures: &mut Vec<String>) {
    for key in ["packages", "services"] {
        let names = |result: &Value| -> Option<std::collections::BTreeSet<String>> {
            Some(
                result["ansible_facts"][key]
                    .as_object()?
                    .keys()
                    .cloned()
                    .collect(),
            )
        };
        let (want, got) = (names(reference), names(ours));
        if want != got {
            let (want, got) = (want.unwrap_or_default(), got.unwrap_or_default());
            let only = |a: &std::collections::BTreeSet<String>,
                        b: &std::collections::BTreeSet<String>| {
                a.difference(b).take(5).cloned().collect::<Vec<_>>()
            };
            failures.push(format!(
                "case {case}: key ansible_facts.{key}: the names differ, only the reference's {:?}, \
                 only ours {:?} (first five each)",
                only(&want, &got),
                only(&got, &want)
            ));
        }
    }
}

/// What a live comparison keeps of `package_facts` and `service_facts`, on both sides: the rest
/// of the machine's packages and units move between the two runs (the apt hook starts
/// `packagekit`, timers fire) for reasons no native controls. One unit per `service_facts`
/// branch: `cron.service` and `systemd-journald.service` from `list-units` (the first with its
/// status replaced by `list-unit-files`), `cron` from the SysV listing, and every template
/// (`name@.service`), which only `list-unit-files` names and whose `show` fails into `unknown`.
#[cfg(target_os = "linux")]
pub fn keep_live(result: &mut Value) {
    let keep: [(&str, &[&str]); 2] = [
        ("packages", &["bash"]),
        (
            "services",
            &["cron.service", "systemd-journald.service", "cron"],
        ),
    ];
    for (key, names) in keep {
        if let Some(Value::Object(map)) = result
            .get_mut("ansible_facts")
            .and_then(|facts| facts.get_mut(key))
        {
            map.retain(|name, _| {
                names.contains(&name.as_str()) || (key == "services" && name.ends_with("@.service"))
            });
        }
    }
}

/// What the generator writes in place of any string naming the reference's staged copy: its
/// `src`, and the archive path `unarchive` quotes in the `tar` command line it ran. Volant's
/// staged copies live under the agent's `remote_tmp`, and a string of ours naming one, or
/// carrying one of the generator's own markers, gets the same placeholder, so both keys are then
/// compared by value: a `src` pointing anywhere else, the user's own file for instance, still
/// differs.
#[cfg(target_os = "linux")]
pub const STAGED_PLACEHOLDER: &str = "<golden-staged-path>";

/// `STAGED_SRC_MARKERS` in `generate.py`.
#[cfg(target_os = "linux")]
pub const STAGED_MARKERS: &[&str] = &["ansible-tmp-", "/tmp/ansible", ".ansible/tmp"];

#[cfg(target_os = "linux")]
pub fn redact_staged(value: &mut Value, staged_root: &str) {
    match value {
        Value::String(s)
            if s.starts_with(staged_root) || STAGED_MARKERS.iter().any(|m| s.contains(m)) =>
        {
            *s = STAGED_PLACEHOLDER.into();
        }
        Value::Array(items) => items.iter_mut().for_each(|v| redact_staged(v, staged_root)),
        Value::Object(map) => map.values_mut().for_each(|v| redact_staged(v, staged_root)),
        _ => {}
    }
}

/// A `slurp` result's content, decoded. `base64 -d`, since no dependency of this crate decodes
/// base64.
#[cfg(target_os = "linux")]
pub fn slurped_text(slurped: &Value) -> Result<Value, String> {
    let encoded = slurped["content"]
        .as_str()
        .ok_or("a slurp result with no content")?;
    let out = std::process::Command::new("sh")
        .args(["-c", "printf %s \"$1\" | base64 -d", "sh", encoded])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("base64 -d refused {encoded}"));
    }
    String::from_utf8(out.stdout)
        .map(Value::from)
        .map_err(|e| e.to_string())
}

/// `_after`, built as `natives()` builds it from the read-back tasks that follow a `file`, `copy`
/// or `lineinfile` case.
#[cfg(target_os = "linux")]
pub fn read_back(case: &str, spec: &Value, results: &Map<String, Value>) -> Result<Value, String> {
    let stat = results
        .get(&format!("after-stat-{case}"))
        .and_then(|result| result.get("stat"))
        .ok_or("no read-back stat")?;
    let mut after = Map::new();
    after.insert("exists".into(), stat["exists"].clone());
    if stat["exists"] == true {
        let kind = if stat["isdir"] == true {
            "directory"
        } else if stat["islnk"] == true {
            "link"
        } else {
            "file"
        };
        after.insert("type".into(), kind.into());
        after.insert("mode".into(), stat["mode"].clone());
    }
    // Skipped unless the path is a regular file, and a skipped task shows no result.
    if let Some(content) = results.get(&format!("after-content-{case}")) {
        after.insert("content".into(), slurped_text(content)?);
    }
    if spec["args"]["backup"] == true {
        let backup = results
            .get(&format!("after-backup-{case}"))
            .ok_or("no read-back of the backup")?;
        after.insert("backup_content".into(), slurped_text(backup)?);
    }
    Ok(Value::Object(after))
}

/// A case's result as Volant gave it: the registered value, which keeps `failed`, with the
/// `invocation` that the reference leaves out of a registered value and the `-vvv` line shows,
/// and `_after` for the modules that change a file.
#[cfg(target_os = "linux")]
pub fn native_result(
    case: &str,
    spec: &Value,
    results: &Map<String, Value>,
) -> Result<Value, String> {
    let shown = results
        .get(case)
        .ok_or("no result line: the task did not run, or did not report")?;
    let mut registered = results
        .get(&format!("registered-{case}"))
        .and_then(|shown| shown.get("last"))
        .cloned()
        .ok_or("no registered value")?;
    let map = registered
        .as_object_mut()
        .ok_or("the registered value is not a mapping")?;
    // The executor sets it on every registered result; the recording's JSON callback, like an
    // `ok:` line, leaves it out. A `true` stays.
    if map.get("failed") == Some(&Value::Bool(false)) {
        map.remove("failed");
    }
    if let Some(invocation) = shown.get("invocation") {
        map.insert("invocation".into(), invocation.clone());
    }
    if ["file", "copy", "lineinfile"].contains(&spec["module"].as_str().unwrap_or_default()) {
        map.insert("_after".into(), read_back(case, spec, results)?);
    }
    if let Some(read) = results.get(&format!("after-unit-{case}")) {
        map.insert("_after".into(), unit_after(read)?);
    }
    Ok(registered)
}

/// `_after` of a unit case, as `natives()` builds it: the `KEY=value` lines of its
/// `after-unit-` read-back (`systemctl show -p ...`).
#[cfg(target_os = "linux")]
pub fn unit_after(read: &Value) -> Result<Value, String> {
    let stdout = read["stdout"]
        .as_str()
        .ok_or("a unit read-back with no stdout")?;
    Ok(Value::Object(
        stdout
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), Value::from(value)))
            .collect(),
    ))
}
