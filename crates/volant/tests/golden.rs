// SPDX-License-Identifier: GPL-3.0-or-later
//! Every case in tests/golden/cases.yml, compared with what the reference ansible-core did.

use serde_json::{Map, Value};
use volant::template::Templar;
use volant::yaml;

#[cfg(target_os = "linux")]
#[path = "common/collections.rs"]
mod collections;

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
            facts: None,
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

/// What one golden comparison loosens: keys compared by type, the by-type keys that may be
/// `null`, and the known differences it steps over, as `(case, path)`.
#[cfg(target_os = "linux")]
struct Rules {
    by_type: &'static [&'static str],
    may_be_null: &'static [&'static str],
    known: &'static [(&'static str, &'static str)],
}

#[cfg(target_os = "linux")]
const MODULE_RULES: Rules = Rules {
    by_type: BY_TYPE,
    may_be_null: MAY_BE_NULL,
    known: KNOWN_DIFFERENCES,
};

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

/// `ok:`/`changed:`/`fatal:` lines at `-v`, keyed by the task banner they arrived under.
#[cfg(target_os = "linux")]
fn results_by_task(stdout: &str) -> Map<String, Value> {
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

/// What a comparison found: the differences, and the known differences it stepped over.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Findings {
    failures: Vec<String>,
    spent: Vec<(String, String)>,
}

#[cfg(target_os = "linux")]
impl Findings {
    /// A known difference whose key neither side carried was never looked at, which is not the
    /// same as still differing: the case stopped reporting that key, or stopped running, and the
    /// entry would otherwise sit there excusing nothing until it excuses something new.
    fn check_spent(&mut self, rules: &Rules) {
        for (case, path) in rules.known {
            if !self.spent.iter().any(|(c, p)| c == case && p == path) {
                self.failures.push(format!(
                    "{case}.{path}: listed as a known difference but neither side has it - delete \
                     the entry"
                ));
            }
        }
    }
}

/// One module's result against the reference's, key by key, walking into `stat`'s nested map.
#[cfg(target_os = "linux")]
fn compare_keys(
    module: &str,
    prefix: &str,
    want: &Map<String, Value>,
    got: &Map<String, Value>,
    id: &Identity,
    rules: &Rules,
    findings: &mut Findings,
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
        if rules.known.contains(&(module, path.as_str())) {
            findings.spent.push((module.to_string(), path.clone()));
            // The exemption is spent here and checked back in below: a difference that has gone
            // away has to be taken off the list, not left to quietly excuse a future one.
            if !matches!((reference, ours), (Some(r), Some(o)) if same(r, o)) {
                continue;
            }
            findings.failures.push(format!(
                "{module}.{path}: listed as a known difference but no longer differs - delete the \
                 KNOWN_DIFFERENCES entry (reference {reference:?}, ours {ours:?})"
            ));
            continue;
        }
        if IF_BOTH.contains(&path.as_str()) && (reference.is_none() || ours.is_none()) {
            continue;
        }
        let (Some(reference), Some(ours)) = (reference, ours) else {
            findings.failures.push(format!(
                "{module}.{path}: reference {reference:?}, ours {ours:?}"
            ));
            continue;
        };
        if let Some(mine) = ownership(&path, id) {
            if !same(ours, mine) {
                findings.failures.push(format!(
                    "{module}.{path}: this account is {mine}, ours reported {ours}"
                ));
            }
            continue;
        }
        if rules.by_type.contains(&path.as_str()) {
            let kind = |v: &Value| std::mem::discriminant(v);
            let nullable = rules.may_be_null.contains(&path.as_str())
                && (reference.is_null() || ours.is_null());
            if !nullable && kind(reference) != kind(ours) {
                findings.failures.push(format!(
                    "{module}.{path}: reference {reference} and ours {ours} are not even the same \
                     kind of value"
                ));
            }
            continue;
        }
        match (reference, ours) {
            (Value::Object(want), Value::Object(got)) => {
                compare_keys(module, &path, want, got, id, rules, findings);
            }
            _ if same(reference, ours) => {}
            _ => findings.failures.push(format!(
                "{module}.{path}: reference {reference}, ours {ours}"
            )),
        }
    }
}

/// The controller interpreter a golden comparison runs Volant with, or `None` after saying loudly
/// why the comparison is skipped. Under `VOLANT_PYTHON` there is no skip: an interpreter that
/// cannot reproduce the recording fails the test.
#[cfg(target_os = "linux")]
fn reference_python() -> Option<std::path::PathBuf> {
    let recorded_version = include_str!("golden/ANSIBLE_VERSION").trim();
    match controller_python(recorded_version) {
        Ok(python) => Some(python),
        Err(why) if std::env::var_os("VOLANT_PYTHON").is_some() => {
            panic!("VOLANT_PYTHON cannot reproduce the recording: {why}")
        }
        Err(why) => {
            eprintln!("skipped: {why}. Set VOLANT_PYTHON to an ansible-core {recorded_version}.");
            None
        }
    }
}

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
    finish_within(&mut command, "volant")
}

/// Starts `command` in a process group of its own and waits for it, or kills the group and panics
/// at a deadline: a play that cannot reach its host waits rather than returning, and a hung test
/// says nothing about the results it was written to compare. The whole group, because the local
/// agent and the modules it starts hold the same pipes, and killing `volant` alone would leave the
/// wait below blocked on them. The native play is the longest: under a minute on the development
/// machine.
#[cfg(target_os = "linux")]
fn finish_within(command: &mut std::process::Command, what: &str) -> std::process::Output {
    use std::os::unix::process::CommandExt as _;
    let deadline = std::time::Duration::from_secs(300);
    let child = command
        .process_group(0)
        .spawn()
        .unwrap_or_else(|e| panic!("{what} starts: {e}"));
    let pid = libc::pid_t::try_from(child.id()).expect("a pid fits a pid_t");
    // Read while waiting: a child whose output outgrows the pipe blocks until someone does,
    // which `package_facts` under the JSON callback does.
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || send.send(child.wait_with_output()));
    let Ok(out) = receive.recv_timeout(deadline) else {
        // Safety: `-pid` is the child's own process group, which it leads and nothing has
        // reaped while the thread waits on it.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let out = receive
            .recv()
            .expect("the waiting thread answers")
            .expect("the child's output");
        panic!(
            "{what} did not finish within {deadline:?}:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    };
    out.expect("the child's output")
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
    let Some(python) = reference_python() else {
        return;
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
    let out = run_recorded_play(dir, "python-modules.yml", &python, &["-v"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let results = results_by_task(&stdout);
    let id = identity();
    let mut findings = Findings::default();
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
            findings.failures.push(format!(
                "{module}: no result at all - the task did not run, or did not report"
            ));
            continue;
        };
        match (reference.as_object(), ours.as_object()) {
            (Some(want), Some(got)) => {
                compare_keys(module, "", want, got, &id, &MODULE_RULES, &mut findings);
            }
            _ => findings
                .failures
                .push(format!("{module}: reference {reference}, ours {ours}")),
        }
    }
    findings.check_spent(&MODULE_RULES);
    let failures = findings.failures;
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

/// The directory `generate.py` recorded the action plugin fixtures in, fixed for the reason
/// `PYTHON_MODULES_DIR` is: `dest`, `path` and `unarchive`'s skip message carry it, and they are
/// compared by value.
#[cfg(target_os = "linux")]
const ACTION_DIR: &str = "/tmp/volant16-golden";

/// The generator's own inputs, which the recording's `checksum`, `md5sum` and `size` were taken
/// from.
#[cfg(target_os = "linux")]
const ACTION_SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/action-src");

/// What the generator writes in place of any string naming the reference's staged copy: its
/// `src`, and the archive path `unarchive` quotes in the `tar` command line it ran. Volant's
/// staged copies live under the agent's `remote_tmp`, and a string of ours naming one, or
/// carrying one of the generator's own markers, gets the same placeholder, so both keys are then
/// compared by value: a `src` pointing anywhere else, the user's own file for instance, still
/// differs.
#[cfg(target_os = "linux")]
const STAGED_PLACEHOLDER: &str = "<golden-staged-path>";

/// `STAGED_SRC_MARKERS` in `generate.py`.
#[cfg(target_os = "linux")]
const STAGED_MARKERS: &[&str] = &["ansible-tmp-", "/tmp/ansible", ".ansible/tmp"];

/// Keys every `systemctl show` of a unit prints, whatever the systemd version.
#[cfg(target_os = "linux")]
const STATUS_FLOOR: &[&str] = &["Id", "LoadState", "ActiveState", "SubState"];

/// `apt`'s `cache_update_time` is the mtime of the host's package cache, a date that moves with
/// every `apt update`. `service`'s `status` is a live `systemctl show` of the unit, recorded with
/// every value replaced by a placeholder, and its key set changes with the systemd version (the
/// recording's has `BindLogSockets` and `CanLiveMount`, which a runner's systemd 255 lacks): by
/// type, with `STATUS_FLOOR` checked on our side.
#[cfg(target_os = "linux")]
const ACTION_BY_TYPE: &[&str] = &["cache_update_time", "status"];

/// Differences this release really has in its action plugins, as `(case, path)`, with the same
/// contract as `KNOWN_DIFFERENCES`: an entry that stops differing fails the test. Each reason is
/// what ansible-core 2.19.12's own source shows.
#[cfg(target_os = "linux")]
const ACTION_KNOWN_DIFFERENCES: &[(&str, &str)] = &[
    // `copy` with `content:` and `force: false` on an existing file: the reference returns
    // `src=source` (`plugins/action/copy.py`), and `source` is the controller-side temporary file
    // it wrote the content to, `tempfile.mkstemp(dir=C.DEFAULT_LOCAL_TMP, prefix='.')`, deleted
    // before the task ends. Volant never writes that file and reports a name of the same shape
    // with no directory, `.9063a9f0` in this run.
    ("copy-force-false", "src"),
];

#[cfg(target_os = "linux")]
const ACTION_RULES: Rules = Rules {
    by_type: ACTION_BY_TYPE,
    may_be_null: &[],
    known: ACTION_KNOWN_DIFFERENCES,
};

#[cfg(target_os = "linux")]
fn redact_staged(value: &mut Value, staged_root: &str) {
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

/// What an action plugin returns is the reference's result, key by key, not a plausible shape.
///
/// Every case `generate.py` recorded is replayed with the same arguments, in the same order, in
/// the same directory, so each one meets the state the previous ones left: `copy-same` finds the
/// file `copy-new` wrote, `copy-force-false` the one `copy-content` wrote. `dest`, `path`, `mode`,
/// `size`, `checksum`, `md5sum`, `state`, `msg` and every other key are compared by value; the
/// ownership keys against the account running the test; `cache_update_time`, `status` and a
/// directory's `size` by type; `invocation` is dropped, and `diff` when it is `[]` on both sides.
/// `unarchive-creates` and `copy-validate-fail` are read back through `register`: Volant prints no
/// result on a `skipping:` line, and a `fatal:` line drops `failed`, `diff` and `exception`.
///
/// `package` and `service` escalate with `sudo -n` and ask systemd. A machine without either
/// skips the test loudly, or fails it under `VOLANT_PYTHON`, which names a job that has to
/// compare everything.
///
/// What would make this red: `copy` answering from its `stat` in the identical branch - the
/// keys look right, `path` is missing; or `template` recording the rendered text's checksum
/// computed another way than the module's.
#[cfg(target_os = "linux")]
#[test]
fn an_action_plugin_returns_the_reference_s_own_keys() {
    let fixtures: [(&str, &str, &str); 14] = [
        (
            "copy-new",
            "copy",
            include_str!("golden/action/copy-new.json"),
        ),
        (
            "copy-same",
            "copy",
            include_str!("golden/action/copy-same.json"),
        ),
        (
            "copy-content",
            "copy",
            include_str!("golden/action/copy-content.json"),
        ),
        (
            "copy-force-false",
            "copy",
            include_str!("golden/action/copy-force-false.json"),
        ),
        (
            "copy-dest-dir",
            "copy",
            include_str!("golden/action/copy-dest-dir.json"),
        ),
        (
            "copy-validate-fail",
            "copy",
            include_str!("golden/action/copy-validate-fail.json"),
        ),
        (
            "copy-remote-src",
            "copy",
            include_str!("golden/action/copy-remote-src.json"),
        ),
        (
            "template-new",
            "template",
            include_str!("golden/action/template-new.json"),
        ),
        (
            "template-same",
            "template",
            include_str!("golden/action/template-same.json"),
        ),
        (
            "template-lstrip",
            "template",
            include_str!("golden/action/template-lstrip.json"),
        ),
        (
            "package-present",
            "package",
            include_str!("golden/action/package-present.json"),
        ),
        (
            "service-started",
            "service",
            include_str!("golden/action/service-started.json"),
        ),
        (
            "unarchive-local",
            "unarchive",
            include_str!("golden/action/unarchive-local.json"),
        ),
        (
            "unarchive-creates",
            "unarchive",
            include_str!("golden/action/unarchive-creates.json"),
        ),
    ];
    let Some(python) = reference_python() else {
        return;
    };
    let missing: Vec<&str> = [
        ("sudo -n true", "passwordless `sudo -n`"),
        ("test -d /run/systemd/system", "running systemd"),
    ]
    .into_iter()
    .filter(|(probe, _)| {
        !std::process::Command::new("sh")
            .args(["-c", probe])
            .output()
            .is_ok_and(|out| out.status.success())
    })
    .map(|(_, what)| what)
    .collect();
    if !missing.is_empty() {
        let why = format!(
            "`package` and `service` were recorded with `become` on systemd, and this machine \
             has no {}",
            missing.join(" and no ")
        );
        assert!(
            std::env::var_os("VOLANT_PYTHON").is_none(),
            "VOLANT_PYTHON cannot reproduce the recording: {why}"
        );
        eprintln!("skipped: {why}.");
        return;
    }

    let dir = std::path::Path::new(ACTION_DIR);
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("the recorded directory is writable");
    // The tasks `action_plugins()` in `generate.py` records, with the same names: the two
    // `setup-<n>` tasks make the directories later cases write into.
    std::fs::write(
        dir.join("action-plugins.yml"),
        format!(
            r#"- hosts: localhost
  gather_facts: false
  tasks:
    - name: copy-new
      copy: {{src: {src}/hello.txt, dest: {dir}/new.txt, mode: "0644"}}
    - name: copy-same
      copy: {{src: {src}/hello.txt, dest: {dir}/new.txt, mode: "0644"}}
    - name: copy-content
      copy: {{content: "x\n", dest: {dir}/content.txt, mode: "0644"}}
    - name: copy-force-false
      copy: {{content: "y\n", dest: {dir}/content.txt, force: false}}
    - name: setup-4
      file: {{path: {dir}/dir, state: directory, mode: "0775"}}
    - name: copy-dest-dir
      copy: {{src: {src}/hello.txt, dest: {dir}/dir/, mode: "0644"}}
    - name: copy-validate-fail
      copy: {{content: "", dest: {dir}/invalid.txt, validate: "test -s %s"}}
      ignore_errors: true
      register: copy_validate_fail
    - name: copy-validate-fail-registered
      debug: {{var: copy_validate_fail}}
    - name: copy-remote-src
      copy: {{src: {dir}/new.txt, dest: {dir}/remote.txt, remote_src: true, mode: "0644"}}
    - name: template-new
      template: {{src: {src}/motd.j2, dest: {dir}/motd, mode: "0644"}}
      vars: {{who: golden, extra: true}}
    - name: template-same
      template: {{src: {src}/motd.j2, dest: {dir}/motd, mode: "0644"}}
      vars: {{who: golden, extra: true}}
    - name: template-lstrip
      template: {{src: {src}/motd.j2, dest: {dir}/motd-lstrip, mode: "0644", lstrip_blocks: true}}
      vars: {{who: golden, extra: true}}
    - name: package-present
      package: {{name: bash, state: present}}
      become: true
      ignore_errors: true
    - name: service-started
      service: {{name: systemd-journald, state: started}}
      become: true
      ignore_errors: true
    - name: setup-13
      file: {{path: {dir}/unpacked, state: directory, mode: "0775"}}
    - name: unarchive-local
      unarchive: {{src: {src}/bundle.tar.gz, dest: {dir}/unpacked}}
    - name: unarchive-creates
      unarchive: {{src: {src}/bundle.tar.gz, dest: {dir}/unpacked, creates: {dir}/unpacked/inside.txt}}
      register: unarchive_creates
    - name: unarchive-creates-registered
      debug: {{var: unarchive_creates}}
"#,
            dir = ACTION_DIR,
            src = ACTION_SRC,
        ),
    )
    .expect("the play is written");
    let out = run_recorded_play(dir, "action-plugins.yml", &python, &["-v"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut results = results_by_task(&stdout);
    // Read back through `register`, where the recording's JSON callback line shows keys the
    // default callback's line leaves out: nothing for a skip, and for a failure the `failed`,
    // `diff` and `exception` a `fatal:` line drops.
    for (case, var) in [
        ("unarchive-creates", "unarchive_creates"),
        ("copy-validate-fail", "copy_validate_fail"),
    ] {
        if let Some(mut shown) = results.remove(&format!("{case}-registered"))
            && let Some(mut registered) = shown.get_mut(var).map(Value::take)
        {
            // A registered result carries the `failed: false` the executor sets on every
            // result; the reference's JSON callback leaves it out, as an `ok:` line does. A
            // `true` stays.
            if let Some(map) = registered.as_object_mut()
                && map.get("failed") == Some(&Value::Bool(false))
            {
                map.remove("failed");
            }
            results.insert(case.into(), registered);
        }
    }
    let staged_root = format!("{ACTION_DIR}/tmp/");
    let id = identity();
    let empty = Value::Array(Vec::new());
    let mut findings = Findings::default();
    for (case, action, recorded) in fixtures {
        let reference: Value = serde_json::from_str(recorded).expect("the fixture parses");
        assert_eq!(
            reference["action"],
            Value::from(action),
            "{case}.json was recorded from a task that ran something else"
        );
        let Some(mut ours) = results.get(case).cloned() else {
            findings.failures.push(format!(
                "{case}: no result at all - the task did not run, or did not report"
            ));
            continue;
        };
        redact_staged(&mut ours, &staged_root);
        let (Value::Object(mut want), Value::Object(mut got)) = (reference.clone(), ours.clone())
        else {
            findings
                .failures
                .push(format!("{case}: reference {reference}, ours {ours}"));
            continue;
        };
        if want.get("diff") == Some(&empty) && got.get("diff") == Some(&empty) {
            want.remove("diff");
            got.remove("diff");
        }
        // A directory's `size` is whatever the filesystem says it takes, 60 on the recording's
        // tmpfs and 4096 on ext4, read by the module and never computed by Volant. A file's
        // `size` stays by value.
        if want.get("state") == Some(&Value::from("directory"))
            && let (Some(reference), Some(ours)) = (want.remove("size"), got.remove("size"))
            && std::mem::discriminant(&reference) != std::mem::discriminant(&ours)
        {
            findings.failures.push(format!(
                "{case}.size: reference {reference} and ours {ours} are not even the same kind of \
                 value"
            ));
        }
        // `status` is compared by type, but an empty object is one too: ours has to be the
        // unit's `systemctl show`, which always names these four.
        if want.contains_key("status") {
            let status = got.get("status").and_then(Value::as_object);
            for key in STATUS_FLOOR {
                if !status.is_some_and(|s| s.contains_key(*key)) {
                    findings.failures.push(format!(
                        "{case}.status.{key}: `systemctl show` always has it, ours {:?}",
                        got.get("status")
                    ));
                }
            }
        }
        compare_keys(case, "", &want, &got, &id, &ACTION_RULES, &mut findings);
    }
    findings.check_spent(&ACTION_RULES);
    let failures = findings.failures;
    let _ = std::fs::remove_dir_all(dir);
    assert!(
        failures.is_empty(),
        "{} key(s) differ from ansible-core across {} case(s):\n{}\n--- volant said\n{stdout}\n--- stderr\n{}",
        failures.len(),
        fixtures.len(),
        failures.join("\n"),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The directory `generate.py`'s `collection_modules()` recorded against, and the one this test
/// runs Volant against. Fixed for the same reason `PYTHON_MODULES_DIR` is: `sysctl`'s
/// `sysctl_file` and `ini_file`'s `path`/`diff` headers carry it, and those are compared by
/// value.
#[cfg(target_os = "linux")]
const COLLECTION_DIR: &str = "/tmp/volant16-golden-collection";

/// Neither recorded module returns a key that moves between runs or machines (no timestamp, no
/// inode, no filesystem attribute), so nothing is loosened: every key compares by value, on the
/// model of `MODULE_RULES` with its lists empty.
#[cfg(target_os = "linux")]
const COLLECTION_RULES: Rules = Rules {
    by_type: &[],
    may_be_null: &[],
    known: &[],
};

/// What an installed collection's own module returns is the reference's result, key by key,
/// exactly like `a_python_module_returns_the_reference_s_own_keys`: neither `ansible.posix.sysctl`
/// nor `community.general.ini_file` is served by an action plugin (A1), so each reaches the host
/// as a plain Python module of the union, and the comparison needs nothing the python-modules test
/// does not already have.
///
/// `sysctl_set` and `reload` are both false: the file `sysctl_file` names is written, and nothing
/// under `/proc/sys` moves. `ini_file` is a module of a different collection with no effect of its
/// own, recorded in the same play so the union this proves carries both without their `sysctl`-
/// shaped short names colliding.
///
/// The version rule mirrors `ANSIBLE_VERSION`'s: a collection this interpreter has, at a version
/// `COLLECTIONS` does not pin, is a named, loud skip; under `VOLANT_PYTHON` it fails instead,
/// because a runner with no collections installed at all would otherwise report every module
/// golden green having compared nothing (A10).
///
/// What would make this red: a collection module reached under its short name instead of its full
/// one, colliding with a builtin or another collection's module of the same name; a result whose
/// keys are plausible and wrong; `ini_file`'s `mode` or ownership read from the wrong field.
#[cfg(target_os = "linux")]
#[test]
fn a_collection_module_returns_the_reference_s_own_keys() {
    let fixtures: [(&str, &str, &str); 2] = [
        (
            "sysctl",
            "ansible.posix.sysctl",
            include_str!("golden/collection/sysctl.json"),
        ),
        (
            "ini_file",
            "community.general.ini_file",
            include_str!("golden/collection/ini_file.json"),
        ),
    ];
    let Some(python) = reference_python() else {
        return;
    };
    let mismatched = collections::collection_mismatches(&python);
    if !mismatched.is_empty() {
        let why = format!(
            "the controller's collections do not match COLLECTIONS: {}",
            mismatched.join("; ")
        );
        assert!(
            std::env::var_os("VOLANT_PYTHON").is_none(),
            "VOLANT_PYTHON cannot reproduce the recording: {why}"
        );
        eprintln!("skipped: {why}.");
        return;
    }

    let dir = std::path::Path::new(COLLECTION_DIR);
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("the recorded directory is writable");
    // The same arguments `generate.py` recorded against, on the same paths, in the same order.
    std::fs::write(
        dir.join("collection-modules.yml"),
        format!(
            "- hosts: localhost\n  gather_facts: false\n  tasks:\n\
             \x20   - name: sysctl\n      ansible.posix.sysctl: {{name: net.ipv4.ip_forward, \
             value: \"1\", sysctl_file: {dir}/sysctl.conf, sysctl_set: false, reload: false}}\n\
             \x20   - name: ini_file\n      community.general.ini_file: {{path: {dir}/test.ini, \
             section: golden, option: color, value: blue, mode: \"0644\"}}\n",
            dir = COLLECTION_DIR
        ),
    )
    .expect("the play is written");
    let out = run_recorded_play(
        dir,
        "collection-modules.yml",
        &python,
        &["-v"],
        &collections_path_env(),
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let results = results_by_task(&stdout);
    let id = identity();
    let mut findings = Findings::default();
    for (name, action, recorded) in fixtures {
        let reference: Value = serde_json::from_str(recorded).expect("the fixture parses");
        assert_eq!(
            reference["action"],
            Value::from(action),
            "{name}.json was recorded from a task that ran something else"
        );
        let Some(ours) = results.get(name) else {
            findings.failures.push(format!(
                "{name}: no result at all - the task did not run, or did not report"
            ));
            continue;
        };
        match (reference.as_object(), ours.as_object()) {
            (Some(want), Some(got)) => {
                compare_keys(name, "", want, got, &id, &COLLECTION_RULES, &mut findings);
            }
            _ => findings
                .failures
                .push(format!("{name}: reference {reference}, ours {ours}")),
        }
    }
    findings.check_spent(&COLLECTION_RULES);
    let failures = findings.failures;
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

/// What `generate.py`'s `natives()` recorded: one file per case, `index.json` saying how each is
/// compared and which path an enabled native takes, and `play.json`, the play that ran them.
const NATIVE_GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/native");

fn native_file(name: &str) -> Value {
    let path = std::path::Path::new(NATIVE_GOLDEN).join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} is readable: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{} parses: {e}", path.display()))
}

/// The recorded play's tasks in order, its block and `always` flattened.
fn native_play_tasks(play: &Value) -> Vec<&Value> {
    let mut tasks = Vec::new();
    for task in play[0]["tasks"].as_array().into_iter().flatten() {
        if task.get("block").is_some() {
            for part in ["block", "always"] {
                tasks.extend(
                    task.get(part)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                );
            }
        } else {
            tasks.push(task);
        }
    }
    tasks
}

/// Every recording in `native/` is indexed, every indexed case has a recording, and the recorded
/// play runs it: a recording the index does not name is compared by nothing, and a case the play
/// never runs has nothing to compare.
///
/// A floor on the set too: every native candidate, `setup` and each alias included, has a case
/// that expects the native to answer, and the two `cron` cases are there. `natives()` leaves out
/// every `become` case on a machine without `sudo -n` and the `cron` ones without cron, and index,
/// recordings and play would still agree with each other.
///
/// What would make this red: a recording left behind by a renamed case, an index entry added by
/// hand, or a regeneration on a machine that could not record a module's cases.
#[test]
fn every_native_recording_is_indexed() {
    let index = native_file("index.json");
    let index = index.as_object().expect("index.json is a mapping");
    let mut failures = Vec::new();
    for module in volant_protocol::modules::NATIVE_CANDIDATES {
        if !index
            .values()
            .any(|spec| spec["module"] == *module && spec["expect"] == "native")
        {
            failures.push(format!(
                "{module}: a native candidate with no indexed case that expects the native to \
                 answer"
            ));
        }
    }
    for case in ["systemd-started-same", "systemd-enabled-only"] {
        if !index.contains_key(case) {
            failures.push(format!("{case}: not indexed (recorded without cron?)"));
        }
    }
    let play = native_file("play.json");
    let run: std::collections::BTreeSet<&str> = native_play_tasks(&play)
        .into_iter()
        .filter_map(|task| task["name"].as_str())
        .collect();
    let recorded: std::collections::BTreeSet<String> = std::fs::read_dir(NATIVE_GOLDEN)
        .expect("native/ is readable")
        .map(|entry| {
            entry
                .expect("native/ lists")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|file| file != "index.json" && file != "play.json")
        .collect();
    for file in &recorded {
        if !file
            .strip_suffix(".json")
            .is_some_and(|case| index.contains_key(case))
        {
            failures.push(format!(
                "native/{file}: every recorded case is indexed, and index.json does not name this \
                 one"
            ));
        }
    }
    for case in index.keys() {
        if !recorded.contains(&format!("{case}.json")) {
            failures.push(format!("{case}: indexed, with no recording"));
        }
        if !run.contains(case.as_str()) {
            failures.push(format!("{case}: indexed, and play.json never runs it"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Where `natives()` recorded, fixed for the reason `PYTHON_MODULES_DIR` is: paths, messages and
/// `_after` carry it, and are compared once both sides read `<golden-tmp>`. Under `/var/tmp`, as
/// in the generator: on the root filesystem rather than a tmpfs, `stat`'s `attributes` and a
/// directory's `size` are the same on every machine these run on, and are compared by value.
#[cfg(target_os = "linux")]
const NATIVE_DIR: &str = "/var/tmp/volant-golden-native";

/// `NATIVE_LOCK` in `generate.py`.
#[cfg(target_os = "linux")]
const NATIVE_LOCK: &str = "/var/tmp/volant-golden-native.lock";

#[cfg(target_os = "linux")]
fn replace_in_strings(value: &mut Value, from: &str, to: &str) {
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
        let id = identity();
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

/// A `slurp` result's content, decoded. `base64 -d`, since no dependency of this crate decodes
/// base64.
#[cfg(target_os = "linux")]
fn slurped_text(slurped: &Value) -> Result<Value, String> {
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
fn read_back(case: &str, spec: &Value, results: &Map<String, Value>) -> Result<Value, String> {
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
fn native_result(case: &str, spec: &Value, results: &Map<String, Value>) -> Result<Value, String> {
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
    Ok(registered)
}

/// An `unordered` value's words, split on single spaces as `sorted(value.split(" "))` does.
#[cfg(target_os = "linux")]
fn words(value: &Value) -> Option<Vec<&str>> {
    let mut words: Vec<&str> = value.as_str()?.split(' ').collect();
    words.sort_unstable();
    Some(words)
}

/// One case's result against the reference's, key by key, `invocation` included, loosened only
/// where the case's index entry says: a `volatile` path is on both sides and its value is not
/// compared, a `patterns` path matches its regex, an `unordered` path is compared as the sorted
/// list of its space-separated words.
#[cfg(target_os = "linux")]
fn compare_native(
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
        // returns (see `DROPPED`). An `_ansible_` key only the recording has is executor
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

/// What `VOLANT_PROFILE_JSON` says of a run on `localhost`: the natives its agent declared, and
/// one line per task the agent ran.
#[cfg(target_os = "linux")]
struct NativeProfile {
    natives: Option<Vec<String>>,
    tasks: Vec<Value>,
}

#[cfg(target_os = "linux")]
fn native_profile(path: &std::path::Path) -> NativeProfile {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("VOLANT_PROFILE_JSON left no {}: {e}", path.display()));
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a profile line is JSON"))
        .collect();
    let natives = lines
        .first()
        .and_then(|first| first["natives"]["localhost"].as_array())
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });
    let tasks = lines
        .into_iter()
        .filter(|line| line.get("task").is_some() && line["host"] == "localhost")
        .collect();
    NativeProfile { natives, tasks }
}

/// The path the case's own module took, against the one it must take: the index's when the agent
/// declared a native for the module, `python` otherwise. A plugin's sub-tasks (`copy`'s `stat`)
/// have lines of their own under the same task and are not the case's.
#[cfg(target_os = "linux")]
fn check_native_path(
    case: &str,
    spec: &Value,
    profile: &NativeProfile,
    failures: &mut Vec<String>,
) {
    let module = spec["module"].as_str().unwrap_or_default();
    let Some(natives) = &profile.natives else {
        failures.push(format!(
            "case {case}: the profile names no natives for localhost, so no path can be required"
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
        if got != want {
            failures.push(if native {
                format!("case {case}: path {got}, index says {want}")
            } else {
                format!("case {case}: path {got}, and {module} is no native of this agent: python")
            });
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
        .env("ANSIBLE_STDOUT_CALLBACK", "ansible.builtin.json")
        .env("ANSIBLE_NOCOLOR", "1")
        .env("ANSIBLE_PYTHON_INTERPRETER", "/usr/bin/python3");
    let out = finish_within(&mut command, "the reference");
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

/// A native module answers what the reference answered, and only where its index entry says it
/// may.
///
/// `play.json`, the play `natives()` recorded, runs under Volant at `-vvv` on the recorded
/// directory, with `--facts native` so `setup` goes to its native whenever the agent has one.
/// Each case's result is its registered value plus the `invocation` of its `-vvv` line (the
/// reference registers none) and the `_after` read back after it, masked as the generator masks
/// the recording. It is compared key by key with `native/<case>.json`, or, for a `live` case, with
/// the reference's own answer: the reference replays the whole play just after, on this machine,
/// since `package_facts`, `service_facts` and a unit's `systemctl show` are the machine's, not the
/// recording's. Both runs get the same scrubbed environment and an empty `ANSIBLE_CONFIG`.
///
/// The path comes from `VOLANT_PROFILE_JSON`. A module among the natives the agent declared must
/// take the index's path (`native` inside the subset, `fallback` outside it); any other takes
/// `python`. So on a build with no native enabled every case is held to `python`, and enabling a
/// native holds each of its cases to the index. A declared native with no case expecting it to
/// answer fails, as does a run that exits non-zero or skips its `always` cleanup.
///
/// Needs an account other than root, `sudo -n`, systemd, apt, cron active and enabled, and no
/// `hello` installed, as the recording did. Without `VOLANT_PYTHON` a missing one skips the test
/// loudly; with it, it fails. Takes the generator's lock for the machine-wide state it changes.
///
/// What would make this red: a native answering a key, a value, a message or a mode the
/// reference does not; a native leaving the file behind differently (`_after`); a native that
/// hands back where the index says it answers, or answers where the index says it hands back; a
/// native that never changes anything (the unit and package cases ask for changes).
#[cfg(target_os = "linux")]
#[test]
fn a_native_module_returns_the_reference_s_own_keys() {
    let Some(python) = reference_python() else {
        return;
    };
    assert!(
        identity().uid != Value::from(0),
        "run as root: the recording's literal `root` would read as the running account's own; run \
         as an ordinary account with passwordless `sudo -n`"
    );
    let missing: Vec<&str> = [
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
    .map(|(_, what)| what)
    .collect();
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
    // The fixture, the accounts, the unit and the package are the machine's: one run at a time,
    // this test's or the generator's.
    let lock = std::fs::File::options()
        .create(true)
        .append(true)
        .open(NATIVE_LOCK)
        .expect("the lock file opens");
    // Safety: a valid descriptor, held open until the end of the test.
    let locked = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) };
    assert_eq!(locked, 0, "{NATIVE_LOCK} is lockable");

    let index = native_file("index.json");
    let index = index.as_object().expect("index.json is a mapping");
    let mut recorded = native_file("play.json");
    replace_in_strings(&mut recorded, "<golden-tmp>", NATIVE_DIR);
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
    // Outside NATIVE_DIR, as in the generator: `stat-dir` reports the directory's link count.
    let work = std::env::temp_dir().join(format!("volant-golden-native-{}", std::process::id()));
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
    let out = run_recorded_play(
        &work,
        "natives.yml",
        &python,
        &["-vvv", "--facts", "native"],
        &[
            ("VOLANT_PROFILE_JSON", profile.display().to_string()),
            (
                "ANSIBLE_CONFIG",
                work.join("ansible.cfg").display().to_string(),
            ),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let results = results_by_task(&stdout);
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
        }
    }
    // The reference replays the whole recorded play from the same fixture, so each live case
    // meets the state the cases before it left.
    native_fixture();
    let live = reference_results(&python, &work, &recorded);
    let masks = NativeMasks::new(format!("{}/tmp/", work.display()));
    let profile = native_profile(&profile);
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
    for (case, spec) in index {
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
                masks.apply(&mut reference);
                keep_live(&mut reference);
                keep_live(&mut ours);
                reference
            }
            Some("exact" | "keys") => native_file(&format!("{case}.json")),
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
    drop(lock);
    assert!(
        failures.is_empty(),
        "{} difference(s) across {} native case(s):\n{}\n--- volant said\n{stdout}\n--- stderr\n{}",
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

/// What `natives()` keeps of `package_facts` and `service_facts` (`LIVE_KEEP`), kept on both
/// sides of a live comparison: the rest of the machine's packages and units move between the two
/// runs (the apt hook starts `packagekit`, timers fire) for reasons no native controls.
#[cfg(target_os = "linux")]
fn keep_live(result: &mut Value) {
    let keep: [(&str, &[&str]); 2] = [
        ("packages", &["bash"]),
        ("services", &["cron.service", "systemd-journald.service"]),
    ];
    for (key, names) in keep {
        if let Some(Value::Object(map)) = result
            .get_mut("ansible_facts")
            .and_then(|facts| facts.get_mut(key))
        {
            map.retain(|name, _| names.contains(&name.as_str()));
        }
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

/// The path check of `a_native_module_returns_the_reference_s_own_keys`, against a native that
/// really answers: `volant_echo`, which exists only in an agent built with the `test-natives`
/// feature (`just ssh-test` builds one and names its directory in
/// `VOLANT_TEST_NATIVES_AGENT_DIR`).
///
/// The controller sends a task to a native only for ansible-core's own module, so the test builds
/// a view of the controller's ansible-core with a `volant_echo.py` among its modules: every entry
/// a link to the real one, but `modules/` a directory of its own. `echo-native` is answered by the
/// native; `echo-fallback` asks the native to hand back, and the Python module answers.
///
/// Under an index that says so, both are green. Under one that says `echo-native` hands back, the
/// same run is red on the path, and on nothing else.
#[cfg(target_os = "linux")]
#[test]
fn a_native_answer_is_held_to_the_path_its_index_names() {
    let Some(python) = reference_python() else {
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
    let profile = native_profile(&profile);
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
                Ok(ours) => compare_native(
                    case,
                    spec,
                    "",
                    recorded[case]
                        .as_object()
                        .expect("a recording is a mapping"),
                    ours.as_object().expect("a result is a mapping"),
                    &mut failures,
                ),
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
