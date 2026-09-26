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
/// at `-v`, and returns once it exits or panics at a deadline.
///
/// `extra_env` is set on the child after every `ANSIBLE_*` variable is stripped from it, so a
/// caller can forward the one or two names it actually needs (`collections_path_env()`) without
/// reopening the door to the rest of this process's own `ANSIBLE_*` environment.
#[cfg(target_os = "linux")]
fn run_recorded_play(
    dir: &std::path::Path,
    playbook: &str,
    python: &std::path::Path,
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
        .arg("-v")
        .arg(dir.join(playbook))
        .env("NO_COLOR", "1")
        .env_remove("COLUMNS")
        .env("VOLANT_PYTHON", python)
        // The local agent inherits this run's environment and caches the payload under this
        // directory, so the wipe before the run takes the cache with it rather than leaving it
        // in the shared one.
        .env("VOLANT_REMOTE_TMP", &remote_tmp)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (name, _) in std::env::vars() {
        if name.starts_with("ANSIBLE_") {
            command.env_remove(name);
        }
    }
    for (name, value) in extra_env {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("volant starts");
    // A play that cannot reach its host waits rather than returning, and a hung test says
    // nothing about the results it was written to compare.
    let deadline = std::time::Duration::from_secs(120);
    let started = std::time::Instant::now();
    loop {
        match child.try_wait().expect("volant is waitable") {
            Some(_) => return child.wait_with_output().expect("volant output"),
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
    let out = run_recorded_play(dir, "python-modules.yml", &python, &[]);
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
    let out = run_recorded_play(dir, "action-plugins.yml", &python, &[]);
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
