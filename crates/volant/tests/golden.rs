// SPDX-License-Identifier: GPL-3.0-or-later
//! Every case in tests/golden/cases.yml, compared with what the reference ansible-core did.

use serde_json::{Map, Value};
use volant::template::Templar;
use volant::yaml;

fn expected() -> Vec<Value> {
    serde_json::from_str(include_str!("golden/expected.json")).expect("expected.json parses")
}

/// Python's `2 == 2.0` is true; serde_json's is not. Integral floats compare equal to integers.
fn same(a: &Value, b: &Value) -> bool {
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

/// `same` above is deliberately order-insensitive. This one is not: it walks two already-`same`
/// values and additionally requires every mapping to iterate its keys in the same order, so
/// `our_yaml_loading_of_vars_matches_the_reference` can actually gate key order through our own
/// `yaml::load`/`to_json` instead of only through `Map`'s order-blind `PartialEq`.
fn same_key_order(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same_key_order(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.keys().eq(y.keys())
                && x.values()
                    .zip(y.values())
                    .all(|(a, b)| same_key_order(a, b))
        }
        _ => true,
    }
}

#[test]
fn every_golden_case_matches_the_reference() {
    let base = std::env::temp_dir().join(format!("volant-golden-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("golden_lookup.txt"), "file contents").unwrap();
    // Safety: nextest runs each test in its own process; nothing else reads the environment here.
    unsafe { std::env::set_var("VOLANT_GOLDEN_ENV", "golden-env-value") };
    let templar = Templar::new(base.clone());
    let mut failures = Vec::new();
    for entry in expected() {
        let case = &entry["case"];
        let mut vars: Map<String, Value> = case
            .get("vars")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        // A case's `untrusted` names are the ones the reference got from a managed host, through
        // a `command` the generator registers and carries into a `set_fact`. On this side they
        // are ordinary values with their names in the untrusted set, which is the same state the
        // executor hands a render after a `register`.
        let untrusted: std::collections::BTreeSet<String> = case
            .get("untrusted")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(k, v)| {
                vars.insert(k.clone(), v.clone());
                k.clone()
            })
            .collect();
        let vars = volant::template::Vars {
            map: &vars,
            hostvars: None,
            shared: None,
            untrusted: Some(&untrusted),
            untrusted_hosts: None,
        };
        let ours: Result<Option<Value>, String> =
            if let Some(when) = case.get("when").and_then(Value::as_str) {
                templar
                    .condition(when, vars)
                    .map(|ok| ok.then(|| Value::String("ran".into())))
                    .map_err(|e| e.to_string())
            } else {
                let text = case["template"].as_str().expect("template is a string");
                templar
                    .render(text, vars)
                    .map(Some)
                    .map_err(|e| e.to_string())
            };
        let verdict = match (
            &ours,
            entry.get("error"),
            entry.get("skipped"),
            entry.get("result"),
        ) {
            (Err(_), Some(_), _, _) | (Ok(None), _, Some(_), _) => Ok(()),
            (Ok(Some(v)), None, None, Some(want)) if same(v, want) => Ok(()),
            _ => Err(format!(
                "{case}\n  reference: {}\n  ours: {ours:?}",
                serde_json::to_string(&entry).unwrap()
            )),
        };
        if let Err(text) = verdict {
            failures.push(text);
        }
    }
    std::fs::remove_dir_all(&base).ok();
    assert!(
        failures.is_empty(),
        "{} case(s) differ from ansible-core:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// `every_golden_case_matches_the_reference` above takes each case's `vars` straight from
/// `expected.json`, i.e. already typed by ansible-core's own YAML reading of `cases.yml` — it
/// never runs `vars` through our own `yaml::to_json`. This test does: it re-parses
/// `cases.yml`'s literal source, converts each case's `vars` mapping through the same
/// `yaml::load`/`yaml::to_json` path a real playbook's `vars_files` or inline `vars` uses, and
/// compares the result to the same reference recording, so a YAML 1.1/1.2 scalar-resolution
/// gap between saphyr and PyYAML shows up here even though the test above can't see it.
#[test]
fn our_yaml_loading_of_vars_matches_the_reference() {
    let cases_text = include_str!("golden/cases.yml");
    let docs = yaml::load(cases_text, "cases.yml").expect("cases.yml parses");
    let cases = docs[0].as_vec().expect("cases.yml is a list of cases");
    let mut failures = Vec::new();
    for (case, entry) in cases.iter().zip(expected()) {
        let Some(vars) = yaml::field(case, "vars") else {
            continue;
        };
        let ours = yaml::to_json(vars).expect("vars convert to JSON");
        let want = entry["case"]["vars"].clone();
        if !same(&ours, &want) {
            failures.push(format!("{case:?}\n  reference: {want}\n  ours: {ours}"));
        } else if !same_key_order(&ours, &want) {
            failures.push(format!(
                "{case:?}\n  key order differs\n  reference: {want}\n  ours: {ours}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} case(s)' vars differ from ansible-core's own YAML reading:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Every invocation in `tests/golden/listing/args.txt`, compared with ansible-core's own output
/// byte for byte.
///
/// This is the widest gate in the suite and the cheapest. The four listing commands print what
/// a compiled play *is*, so the recording covers the things behavioural tests reach only
/// indirectly: the order roles and their dependencies are spliced in, the `role : name` prefix
/// and its absence on an unnamed task, an `import_tasks` expanded where it stands while an
/// `include_tasks` stays one task, an `import_playbook` numbering its plays into the file that
/// read it, the tags a block hands down, and which tasks a `--tags` leaves out.
///
/// Two of the cases carry no output at all and exist for their exit code: a listing skips the
/// pre-flight, and these say it does not thereby skip the loader (`notaplay.yml`, exit 4) or the
/// compilation (`missing-role.yml`, exit 1). A release that answered a listing before reading
/// the file would pass every other case here and fail these two.
///
/// What would make this red: a space, a tab, a tag out of order, a `never` task listed, an
/// include expanded, a role prefix on a task that has no name of its own, a `--limit` that
/// filters nothing, an `import_tasks` inside a role resolved against the wrong directory - and
/// any of the compilation differences above.
///
/// The fixtures avoid the two places the reference is not reproducible: a play with two tags
/// and a play matching two hosts both print out of a Python set, whose iteration order changes
/// between processes. `generate.py`'s `listings()` refuses to record either.
#[test]
fn every_listing_matches_the_reference_byte_for_byte() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/listing");
    let expected: Map<String, Value> =
        serde_json::from_str(include_str!("golden/expected_listing.json"))
            .expect("expected_listing.json parses");
    let mut failures = Vec::new();
    for (line, want) in &expected {
        // `shlex`, because `generate.py` recorded the line with `shlex.split`. Splitting on
        // whitespace here agrees with it until the first quoted argument and then hands the
        // engine two half-arguments while the recording holds one whole one.
        let args = shlex::split(line).unwrap_or_else(|| panic!("{line} splits like a shell"));
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_volant"));
        command
            .arg("playbook")
            .args(["-i", "inv.ini"])
            .args(&args)
            .current_dir(&dir)
            .env("NO_COLOR", "1")
            .env_remove("COLUMNS");
        // The recording was made with the whole `ANSIBLE_` prefix cleared, not a chosen few
        // names: any one of them exported on the machine running the tests would change the
        // answer for a reason that is not the engine's.
        for (name, _) in std::env::vars() {
            if name.starts_with("ANSIBLE_") {
                command.env_remove(name);
            }
        }
        let out = command.output().expect("volant runs");
        let got = String::from_utf8_lossy(&out.stdout);
        let wanted = want["stdout"].as_str().expect("a recorded stdout");
        let code = want["code"].as_i64().map(|c| c as i32);
        if got != wanted || out.status.code() != code {
            failures.push(format!(
                "{line}\n--- reference (exit {code:?})\n{wanted}\n--- volant (exit {:?})\n{got}\n--- stderr\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} listing(s) differ from ansible-core:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn inventory_matches_the_reference() {
    let expected: Value =
        serde_json::from_str(include_str!("golden/expected_inventory.json")).unwrap();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/inventory.ini");
    let inv = volant::inventory::Inventory::load(&path).unwrap();
    let mut failures = Vec::new();

    for (host, want) in expected["hostvars"].as_object().unwrap() {
        let got = inv.host_with_vars(host).vars;
        // Every variable the reference reports, `ansible_*` included. This used to skip all but
        // `ansible_host` and `ansible_connection`, which left the connection variables this
        // release reads - the port, the user, the key, the ssh arguments, `remote_tmp` and the
        // `become` pair - ungated against `ansible-inventory`'s own reading of the same file.
        // The fixture now sets them, so the comparison has something to fail on.
        for (k, v) in want.as_object().unwrap() {
            match got.get(k) {
                Some(ours) if same(ours, v) => {}
                other => failures.push(format!("{host}.{k}: reference {v}, ours {other:?}")),
            }
        }
    }
    for (group, want) in expected["groups"].as_object().unwrap() {
        let mut got = inv.groups().get(group).cloned().unwrap_or_default();
        got.sort();
        if serde_json::to_value(&got).unwrap() != *want {
            failures.push(format!("group {group}: reference {want}, ours {got:?}"));
        }
    }
    for (pattern, want) in expected["patterns"].as_object().unwrap() {
        let res = inv.resolve(pattern);
        let got: Vec<&str> = res.hosts.iter().map(|h| h.name.as_str()).collect();
        if serde_json::to_value(&got).unwrap() != want["hosts"] {
            failures.push(format!(
                "pattern {pattern}: reference {}, ours {got:?}",
                want["hosts"]
            ));
        }
        // inventory.ini deliberately carries no host/group homonym (see homonym_inventory.ini
        // below), so every one of these patterns must come back warning-free; checking both
        // directions means a spurious warning fails this just as loudly as a missing one, unlike
        // the one-directional check this replaced, which passed regardless of whether `resolve`
        // warned correctly because this fixture used to warn on every single pattern.
        match (want["warning"].as_bool(), res.warnings.is_empty()) {
            (Some(true), true) => {
                failures.push(format!("pattern {pattern}: the reference warns, we do not"));
            }
            (Some(false), false) => failures.push(format!(
                "pattern {pattern}: we warn, the reference does not: {:?}",
                res.warnings
            )),
            _ => {}
        }
    }

    // The one case in this golden that must warn: a group and one of its own hosts share a name.
    // Kept in its own tiny fixture (see homonym_inventory.ini's own comment) precisely so it does
    // not drown out the "must not warn" signal above.
    let homonym_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/homonym_inventory.ini");
    let homonym_inv = volant::inventory::Inventory::load(&homonym_path).unwrap();
    let homonym_res = homonym_inv.resolve("same");
    let homonym_got: Vec<&str> = homonym_res.hosts.iter().map(|h| h.name.as_str()).collect();
    if serde_json::to_value(&homonym_got).unwrap() != expected["homonym"]["hosts"] {
        failures.push(format!(
            "homonym pattern same: reference {}, ours {homonym_got:?}",
            expected["homonym"]["hosts"]
        ));
    }
    if expected["homonym"]["warning"].as_bool() == Some(true) && homonym_res.warnings.is_empty() {
        failures.push("homonym pattern same: the reference warns, we do not".to_string());
    }

    assert!(
        failures.is_empty(),
        "{} difference(s) with ansible-inventory:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The directory `generate.py` recorded the Python module fixtures against, and the one this
/// test runs Volant against.
///
/// It is the generator's own fixed path on purpose, not a temporary directory: `file`'s `dest`,
/// `stat`'s `path` and all four of `lineinfile`'s diff headers carry it, and those headers are
/// the only content `lineinfile`'s recording has. Against a `tempdir` every one of them is a
/// difference that is not a bug, and the repair that suggests itself - ignoring the keys that
/// carry a path - leaves `lineinfile` asserting nothing worth asserting. Anyone tempted to turn
/// this into a `tempdir` has to delete that assertion first, and should not.
///
/// It is wiped before the run for the reason the generator wipes it: `lineinfile` is
/// idempotent, so a file left behind by an earlier run turns the recorded `line added` into a
/// no-change result, and the comparison goes red for a reason that is not a bug either.
#[cfg(target_os = "linux")]
const PYTHON_MODULES_DIR: &str = "/tmp/volant15-golden";

/// The account this test process runs as, resolved now rather than read from the fixture.
///
/// The recording replaces the eight ownership values with a placeholder, because the account
/// that ran the generator must not appear in the repository. The placeholder says "resolve this
/// locally", not "accept whatever came back": a `file` result naming `root` because it escalated
/// where the reference did not, or `nobody` because it read the wrong field, has to compare wrong
/// against the account actually running, or these keys prove nothing.
#[cfg(target_os = "linux")]
struct Identity {
    uid: Value,
    gid: Value,
    user: Value,
    group: Value,
}

#[cfg(target_os = "linux")]
fn identity() -> Identity {
    let out = std::process::Command::new("sh")
        .args(["-c", "id -u; id -g; id -un; id -gn"])
        .output()
        .expect("id runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = text.lines().map(str::trim).collect();
    let [uid, gid, user, group] = fields[..] else {
        panic!("id printed four lines, got {fields:?}");
    };
    Identity {
        uid: Value::from(uid.parse::<u64>().expect("a numeric uid")),
        gid: Value::from(gid.parse::<u64>().expect("a numeric gid")),
        user: Value::from(user),
        group: Value::from(group),
    }
}

/// A controller interpreter importing exactly the ansible-core `want` names, or why there is none.
///
/// Exactly, because the payload Volant sends is built from the controller's own ansible-core, so
/// a different version sends a different module: GitHub's runner image ships one whose `stat`
/// returns `disk_usage_bytes`, which 2.19.12's does not. Comparing across versions measures the
/// gap between them and passes or fails for reasons that have nothing to do with Volant, which is
/// why `generate.py` refuses to record against any other version and this refuses to compare.
///
/// An explicit `VOLANT_PYTHON` is the only candidate when it is set, as it is for
/// `python::resolve`. Otherwise the same order that function uses, plus the interpreter beside an
/// `ansible-playbook` on `PATH`, which is where a `uv tool` or `pipx` install puts one. Nothing
/// here names a path: an account's home directory must not reach the repository, and a hard-coded
/// candidate would be exactly that on the machine this was written against.
#[cfg(target_os = "linux")]
fn controller_python(want: &str) -> Result<std::path::PathBuf, String> {
    let candidates: Vec<std::path::PathBuf> =
        if let Some(explicit) = std::env::var_os("VOLANT_PYTHON") {
            vec![explicit.into()]
        } else {
            let mut candidates = Vec::new();
            if let Some(venv) = std::env::var_os("VIRTUAL_ENV") {
                candidates.push(std::path::Path::new(&venv).join("bin/python"));
            }
            let probe = std::process::Command::new("sh")
                .args(["-c", "command -v ansible-playbook"])
                .output()
                .expect("command -v runs");
            let named = String::from_utf8_lossy(&probe.stdout).trim().to_string();
            if !named.is_empty()
                && let Ok(real) = std::fs::canonicalize(&named)
                && let Some(bin) = real.parent()
            {
                candidates.push(bin.join("python3"));
                candidates.push(bin.join("python"));
            }
            candidates.push("python3".into());
            candidates
        };
    let mut seen = Vec::new();
    for python in candidates {
        let Ok(out) = std::process::Command::new(&python)
            .args([
                "-c",
                "from ansible.release import __version__; print(__version__)",
            ])
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if version == want {
            return Ok(python);
        }
        seen.push(format!("{} has ansible-core {version}", python.display()));
    }
    Err(if seen.is_empty() {
        format!("the recording is ansible-core {want} and no interpreter here can import any")
    } else {
        format!(
            "the recording is ansible-core {want}, and comparing against another version would \
             measure the version gap: {}",
            seen.join("; ")
        )
    })
}

/// Keys neither side is asked about, at the path they sit at.
///
/// `invocation` because the reference's own callback drops it and the recording therefore has
/// none, while the module's raw stdout carries one. `action` because it is not something a module
/// returns at all - ansible-core's JSON callback copies it off the *task*, so requiring it would
/// ask Volant to invent a key to satisfy a fixture, which is the test dictating the
/// implementation. What `action` was there to prove is proved better below, by which task banner
/// the result arrived under and by asserting on the reference side that it names the module the
/// fixture claims to be.
#[cfg(target_os = "linux")]
const DROPPED: &[&str] = &["invocation", "action"];

/// Compared only when both sides carry the key.
///
/// `_ansible_no_log` comes from the same executor bookkeeping as `invocation`. It is `false` in
/// every fixture and can only be anything else under `no_log: true`, which no recorded task uses,
/// so a by-value comparison could go red in exactly one situation: Volant declining to emit an
/// ansible-core internal for a task that never asked for it. The fix that would suggest itself is
/// hard-coding `false` into Volant's output to please the fixture.
#[cfg(target_os = "linux")]
const IF_BOTH: &[&str] = &["_ansible_no_log"];

/// Compared by type, never by value: these move between runs, between filesystems or between
/// machines.
///
/// Every key 2.19.12's `stat` returns was read against its source for this list, not only the
/// ones a run happened to trip on. `atime`, `ctime`, `mtime`, `inode` and `dev` move between
/// runs; `attr_flags`, `attributes`, `version`, `block_size`, `blocks` and `nlink` depend on the
/// filesystem - `/tmp` on tmpfs has no `lsattr` attributes at all, on ext4 it has `e`; `mimetype`
/// and `charset` read `unknown` without `file(1)`. `mode` and the twelve permission bits read
/// from it are *not* here: they depend on nothing but the target's mode, which both sides now set
/// explicitly, so they are compared by value like `file`'s own explicit `0644`. The same goes for
/// the three `os.access` keys (read, write, execute), which follow from that mode and from the
/// target being the running account's own file.
///
/// The paths are qualified so a rule written for `stat`'s nested map cannot loosen a top-level
/// key of the same name. The apt-only movers the handoff names - `cache_update_time`, `version`,
/// `stdout`, `stdout_lines`, `stderr`, `stderr_lines` and `diff` - are absent because apt is not
/// compared here at all (see the test's own comment); they belong here, unqualified, when it is.
#[cfg(target_os = "linux")]
const BY_TYPE: &[&str] = &[
    "size",
    "stat.atime",
    "stat.attr_flags",
    "stat.attributes",
    "stat.block_size",
    "stat.blocks",
    "stat.charset",
    "stat.checksum",
    "stat.ctime",
    "stat.dev",
    "stat.inode",
    "stat.mimetype",
    "stat.mtime",
    "stat.nlink",
    "stat.size",
    "stat.version",
];

/// By-type keys where `null` on either side is as good as a match.
///
/// `version` is the inode generation number `lsattr -v` reports, `None` where the filesystem has
/// none: the recording's `/tmp` is tmpfs and says `null`, a runner's `/tmp` on ext4 says a
/// number. Only this key is loosened, so an `atime` that came back `null` still fails.
#[cfg(target_os = "linux")]
const MAY_BE_NULL: &[&str] = &["stat.version"];

/// Differences this release really has, which the comparison steps over and then insists are
/// still there.
///
/// None at the moment. An entry is not a way to look away from a difference:
/// `a_python_module_returns_the_reference_s_own_keys` fails if a listed difference has stopped
/// differing, so whoever fixes the executor is sent straight here to delete the line, and the
/// exemption cannot outlive the defect.
#[cfg(target_os = "linux")]
const KNOWN_DIFFERENCES: &[(&str, &str)] = &[];

/// The eight ownership keys, and which part of the running account each one must equal.
#[cfg(target_os = "linux")]
fn ownership<'a>(path: &str, id: &'a Identity) -> Option<&'a Value> {
    match path {
        "owner" | "stat.pw_name" => Some(&id.user),
        "group" | "stat.gr_name" => Some(&id.group),
        "uid" | "stat.uid" => Some(&id.uid),
        "gid" | "stat.gid" => Some(&id.gid),
        _ => None,
    }
}

/// `ok:`/`changed:` lines at `-v`, keyed by the task banner they arrived under.
#[cfg(target_os = "linux")]
fn results_by_task(stdout: &str) -> Map<String, Value> {
    let mut results = Map::new();
    let mut task = String::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("TASK [")
            && let Some(end) = rest.find(']')
        {
            task = rest[..end].to_string();
        } else if (line.starts_with("ok: [") || line.starts_with("changed: ["))
            && let Some((_, json)) = line.split_once(" => ")
            && let Ok(value) = serde_json::from_str(json)
        {
            results.insert(task.clone(), value);
        }
    }
    results
}

/// One module's result against the reference's, key by key, walking into `stat`'s nested map.
#[cfg(target_os = "linux")]
fn compare_keys(
    module: &str,
    prefix: &str,
    want: &Map<String, Value>,
    got: &Map<String, Value>,
    id: &Identity,
    failures: &mut Vec<String>,
) {
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
        if DROPPED.contains(&path.as_str()) {
            continue;
        }
        if KNOWN_DIFFERENCES.contains(&(module, path.as_str())) {
            // The exemption is spent here and checked back in below: a difference that has gone
            // away has to be taken off the list, not left to quietly excuse a future one.
            if reference.is_some() && ours.is_none() {
                continue;
            }
            failures.push(format!(
                "{module}.{path}: listed as a known difference but no longer differs - delete the \
                 KNOWN_DIFFERENCES entry (reference {reference:?}, ours {ours:?})"
            ));
            continue;
        }
        if IF_BOTH.contains(&path.as_str()) && (reference.is_none() || ours.is_none()) {
            continue;
        }
        let (Some(reference), Some(ours)) = (reference, ours) else {
            failures.push(format!(
                "{module}.{path}: reference {reference:?}, ours {ours:?}"
            ));
            continue;
        };
        if let Some(mine) = ownership(&path, id) {
            if !same(ours, mine) {
                failures.push(format!(
                    "{module}.{path}: this account is {mine}, ours reported {ours}"
                ));
            }
            continue;
        }
        if BY_TYPE.contains(&path.as_str()) {
            let kind = |v: &Value| std::mem::discriminant(v);
            let nullable =
                MAY_BE_NULL.contains(&path.as_str()) && (reference.is_null() || ours.is_null());
            if !nullable && kind(reference) != kind(ours) {
                failures.push(format!(
                    "{module}.{path}: reference {reference} and ours {ours} are not even the same \
                     kind of value"
                ));
            }
            continue;
        }
        match (reference, ours) {
            (Value::Object(want), Value::Object(got)) => {
                compare_keys(module, &path, want, got, id, failures);
            }
            _ if same(reference, ours) => {}
            _ => failures.push(format!(
                "{module}.{path}: reference {reference}, ours {ours}"
            )),
        }
    }
}

/// A payload that executes is not a payload that is correct. The proof is key by key against
/// what the reference returned for the same arguments, on the same paths.
///
/// A shape check would not do. While this milestone was being measured, three modules in a row
/// came back with valid JSON reading `{"failed": true, "msg": "missing required arguments:
/// path"}` - a plausible shape, every argument silently dropped before the task was compiled, and
/// nothing about the result said so. A test that asks whether JSON came back accepts that, and a
/// playbook reads the answer wrongly afterwards.
///
/// Four of the five recorded modules run here. `apt` does not: its fixture was recorded under
/// `check_mode: true`, which this release refuses by name at the pre-flight, and the only other
/// way to reach that result would be to let the test install a package for real. When check mode
/// lands, the task goes back in and the apt-only keys join `BY_TYPE` unqualified - the comparison
/// itself needs nothing else. `apt.json` is not read at all meanwhile, which is also why a
/// regeneration on a non-Debian machine, where the generator deletes it, cannot break this build.
///
/// Without `VOLANT_PYTHON` the test skips, loudly, when no interpreter here imports the recorded
/// ansible-core: building a module payload is the reference's own job and there is no offline
/// stand-in for it. With `VOLANT_PYTHON` set it never skips - a wrong version or no ansible-core
/// at all fails instead - so a job that names its interpreter cannot go green without having
/// compared anything. `just ssh-test` is that job.
///
/// What would make this red: a module reached with no arguments, or with the wrong ones; a result
/// whose keys are plausible and wrong; `file` applying a mode other than the one it was given;
/// ownership read from the wrong field or escalated when the reference did not escalate;
/// `lineinfile` reporting a diff against a path it was not pointed at; and a key the reference
/// returns that this release does not, or the other way round.
#[cfg(target_os = "linux")]
#[test]
fn a_python_module_returns_the_reference_s_own_keys() {
    let fixtures: [(&str, &str); 4] = [
        ("ping", include_str!("golden/python-modules/ping.json")),
        ("stat", include_str!("golden/python-modules/stat.json")),
        ("file", include_str!("golden/python-modules/file.json")),
        (
            "lineinfile",
            include_str!("golden/python-modules/lineinfile.json"),
        ),
    ];
    let recorded_version = include_str!("golden/ANSIBLE_VERSION").trim();
    let python = match controller_python(recorded_version) {
        Ok(python) => python,
        Err(why) if std::env::var_os("VOLANT_PYTHON").is_some() => {
            panic!("VOLANT_PYTHON cannot reproduce the recording: {why}")
        }
        Err(why) => {
            eprintln!("skipped: {why}. Set VOLANT_PYTHON to an ansible-core {recorded_version}.");
            return;
        }
    };

    let dir = std::path::Path::new(PYTHON_MODULES_DIR);
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("the recorded directory is writable");
    let target = dir.join("golden-stat-target");
    std::fs::write(&target, "golden stat fixture\n").expect("the stat target is written");
    // The mode `generate.py` gives its own copy: left to the umask, `stat`'s mode and the twelve
    // permission bits read from it would name whoever ran the test.
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))
            .expect("the stat target's mode is set");
    }
    let remote_tmp = dir.join("tmp");
    std::fs::create_dir_all(&remote_tmp).expect("the blob cache directory is writable");
    // The same arguments `generate.py` recorded against, on the same paths, in the same order.
    // `gather_facts: false` because this play wants none: with facts gathered every run would
    // also be timing and comparing a `setup` nothing here asked for.
    std::fs::write(
        dir.join("python-modules.yml"),
        format!(
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n\
             \x20   - name: ping\n      ping: {{}}\n\
             \x20   - name: stat\n      stat: {{path: {dir}/golden-stat-target}}\n\
             \x20   - name: file\n      file: {{path: {dir}/golden-file, state: touch, mode: \"0644\"}}\n\
             \x20   - name: lineinfile\n      lineinfile: {{path: {dir}/golden-line, line: hello, create: true}}\n",
            dir = PYTHON_MODULES_DIR
        ),
    )
    .expect("the play is written");
    std::fs::write(
        dir.join("hosts.ini"),
        "localhost ansible_connection=local\n",
    )
    .expect("the inventory is written");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .arg("playbook")
        .args(["-i", &dir.join("hosts.ini").display().to_string()])
        .arg("-v")
        .arg(dir.join("python-modules.yml"))
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        .env("VOLANT_PYTHON", &python)
        // The local agent inherits this run's environment and caches the payload under this
        // directory, so the wipe above takes the cache with it rather than leaving it in the
        // shared one.
        .env("VOLANT_REMOTE_TMP", &remote_tmp)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (name, _) in std::env::vars() {
        if name.starts_with("ANSIBLE_") {
            command.env_remove(name);
        }
    }
    let mut child = command.spawn().expect("volant starts");
    // A play that cannot reach its host waits rather than returning, and a hung test says
    // nothing about the module results it was written to compare.
    let deadline = std::time::Duration::from_secs(120);
    let started = std::time::Instant::now();
    let out = loop {
        match child.try_wait().expect("volant is waitable") {
            Some(_) => break child.wait_with_output().expect("volant output"),
            None if started.elapsed() >= deadline => {
                let _ = child.kill();
                let out = child.wait_with_output().expect("volant output");
                panic!(
                    "volant did not finish within {deadline:?}:\n{}",
                    String::from_utf8_lossy(&out.stdout)
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let results = results_by_task(&stdout);
    let id = identity();
    let mut failures = Vec::new();
    for (module, recorded) in fixtures {
        let reference: Value = serde_json::from_str(recorded).expect("the fixture parses");
        // The fixture's own `action` is the one thing this comparison asks of that key: a
        // recording taken from the wrong task would otherwise be compared under the right name.
        assert_eq!(
            reference["action"],
            Value::from(module),
            "{module}.json was recorded from a task that ran something else"
        );
        let Some(ours) = results.get(module) else {
            failures.push(format!(
                "{module}: no result at all - the task did not run, or did not report"
            ));
            continue;
        };
        match (reference.as_object(), ours.as_object()) {
            (Some(want), Some(got)) => {
                compare_keys(module, "", want, got, &id, &mut failures);
            }
            _ => failures.push(format!("{module}: reference {reference}, ours {ours}")),
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    assert!(
        failures.is_empty(),
        "{} key(s) differ from ansible-core across {} module(s):\n{}\n--- volant said\n{stdout}\n--- stderr\n{}",
        failures.len(),
        fixtures.len(),
        failures.join("\n"),
        String::from_utf8_lossy(&out.stderr)
    );
}
