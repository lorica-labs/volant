// SPDX-License-Identifier: GPL-3.0-or-later
//! The goldens that need no host: `cases.yml`'s templates, the inventory, every scenario under
//! `tests/scenarios`, and the module index's own consistency.

use rstest::rstest;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use volant::template::Templar;
use volant::yaml;

#[path = "common/compare.rs"]
#[expect(dead_code, reason = "reference_modules.rs and ssh_e2e.rs use the rest")]
mod compare;
use compare::same;
#[cfg(target_os = "linux")]
use compare::{MODULE_GOLDEN, NativeProfile, check_native_path, module_file, play_tasks};

/// The reasons the native `setup` hands back for a host outside its subset, the agent's own list.
#[cfg(target_os = "linux")]
#[path = "../../volant-agent/tests/setup_exits/mod.rs"]
mod setup_exits;

#[path = "common/process.rs"]
mod process;

#[path = "common/python.rs"]
mod python;

/// A scenario run that has not finished by then waits on something that will not come.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

fn expected() -> Vec<Value> {
    serde_json::from_str(include_str!("golden/expected.json")).expect("expected.json parses")
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

/// Every row of `cases.yml` rendered by Volant: the value, the skip, or the failure ansible-core
/// recorded in `expected.json`, with key order compared too.
#[test]
fn every_golden_case_matches_the_reference() {
    let base = std::env::temp_dir().join(format!("volant-golden-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("golden_lookup.txt"), "file contents").unwrap();
    // Safety: nextest runs each test in its own process; nothing else reads the environment here.
    unsafe { std::env::set_var("VOLANT_GOLDEN_ENV", "golden-env-value") };
    let templar = Templar::new(base.clone());
    let mut failures = Vec::new();
    let mut bridged = 0;
    for entry in expected() {
        let case = &entry["case"];
        // Answered by the Python helper, which `reference_modules.rs` runs.
        if case["bridge"] == true {
            bridged += 1;
            continue;
        }
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
    assert!(bridged >= 80, "{bridged} bridge cases: cases.yml lost some");
    assert!(
        failures.is_empty(),
        "{} case(s) differ from ansible-core:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Each case's `vars`, read from `cases.yml` by our own YAML loader, typed as ansible-core typed
/// them in `expected.json`.
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

/// `case.yml` of a scenario, as the generator's `scenario.py` reads it: the argv of each run,
/// `modes`, `env`, `forks`, `needs` and `unordered`.
struct Case {
    runs: Vec<Vec<String>>,
    single: bool,
    env: Vec<(String, String)>,
    forks: u64,
    needs: Vec<String>,
    unordered: Vec<String>,
}

/// The `modes` of a case.yml and the arguments each appends to a run, in the order recorded.
const MODES: [(&str, &[&str]); 3] = [
    ("check", &["--check"]),
    ("check-diff", &["--check", "--diff"]),
    ("diff", &["--diff"]),
];

/// A list of strings, or a refusal naming the key.
fn string_list(value: &Value, what: &str) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("{what} is not a list"))
        .iter()
        .map(|v| {
            v.as_str()
                .unwrap_or_else(|| panic!("{what} holds {v}, not a string"))
                .to_string()
        })
        .collect()
}

impl Case {
    fn parse(name: &str, doc: &Value) -> Self {
        let map = doc
            .as_object()
            .unwrap_or_else(|| panic!("{name}/case.yml is not a mapping"));
        for key in map.keys() {
            assert!(
                [
                    "args",
                    "runs",
                    "modes",
                    "env",
                    "forks",
                    "needs",
                    "unordered"
                ]
                .contains(&key.as_str()),
                "{name}/case.yml: {key} is not supported"
            );
        }
        let mut runs: Vec<Vec<String>> = match (map.get("args"), map.get("runs")) {
            (Some(args), None) => vec![string_list(args, &format!("{name}: args"))],
            (None, Some(runs)) => runs
                .as_array()
                .unwrap_or_else(|| panic!("{name}: runs is not a list"))
                .iter()
                .map(|run| {
                    let run = run
                        .as_object()
                        .filter(|run| run.len() == 1)
                        .and_then(|run| run.get("args"))
                        .unwrap_or_else(|| panic!("{name}: a run is not {{args: [...]}}"));
                    string_list(run, &format!("{name}: a run's args"))
                })
                .collect(),
            _ => panic!("{name}/case.yml: `args` or `runs`, not both or neither"),
        };
        // Each run, then once more per mode, in this fixed order.
        let modes = map
            .get("modes")
            .filter(|v| !v.is_null())
            .map(|v| string_list(v, &format!("{name}: modes")))
            .unwrap_or_default();
        for mode in &modes {
            assert!(
                MODES.iter().any(|(m, _)| m == mode),
                "{name}: modes takes check, check-diff, diff, not {mode}"
            );
        }
        if !modes.is_empty() {
            runs = runs
                .into_iter()
                .flat_map(|run| {
                    let extra = MODES
                        .iter()
                        .filter(|(m, _)| modes.iter().any(|x| x == m))
                        .map(|(_, flags)| {
                            let mut run = run.clone();
                            run.extend(flags.iter().map(ToString::to_string));
                            run
                        })
                        .collect::<Vec<_>>();
                    std::iter::once(run).chain(extra)
                })
                .collect();
        }
        let env = map
            .get("env")
            .filter(|env| !env.is_null())
            .map(|env| {
                env.as_object()
                    .unwrap_or_else(|| panic!("{name}: env is not a mapping"))
                    .iter()
                    .map(|(k, v)| {
                        let v = v
                            .as_str()
                            .unwrap_or_else(|| panic!("{name}: env {k} is not a string"));
                        (k.clone(), v.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let forks = map.get("forks").map_or(1, |forks| {
            forks
                .as_u64()
                .filter(|&forks| forks > 0)
                .unwrap_or_else(|| panic!("{name}: forks is not a positive integer"))
        });
        let list = |key: &str| {
            map.get(key)
                .filter(|v| !v.is_null())
                .map(|v| string_list(v, &format!("{name}: {key}")))
                .unwrap_or_default()
        };
        Self {
            runs,
            single: map.contains_key("args") && modes.is_empty(),
            env,
            forks,
            needs: list("needs"),
            unordered: list("unordered"),
        }
    }

    /// The needs this machine does not meet, and the reference python when the case needs it.
    fn unmet(&self, name: &str) -> (Vec<String>, Option<PathBuf>) {
        let mut unmet = Vec::new();
        let mut python = None;
        for need in &self.needs {
            match need.as_str() {
                "ansible-core" => match python::reference_python() {
                    Some(found) => python = Some(found),
                    None => unmet.push("the recorded ansible-core".to_string()),
                },
                "sudo" => {
                    let ok = std::process::Command::new("sudo")
                        .args(["-n", "true"])
                        .output()
                        .is_ok_and(|out| out.status.success());
                    if !ok {
                        unmet.push("passwordless `sudo -n`".to_string());
                    }
                }
                "systemd" => {
                    if !Path::new("/run/systemd/system").is_dir() {
                        unmet.push("a running systemd".to_string());
                    }
                }
                other => panic!("{name}: needs {other} is none of ansible-core, sudo, systemd"),
            }
        }
        (unmet, python)
    }
}

/// `stdout` with a command's timings and an async job's id and process masked, and the lines an
/// `unordered` regex matches sorted among themselves, each staying in the slots such lines take:
/// only their order is excused.
fn normalise(stdout: &str, unordered: &[regex::Regex]) -> String {
    // A command's timings, which differ between two runs of the reference itself.
    let timed = regex::Regex::new(r#""(start|end|delta)": "[^"]*""#).expect("the pattern compiles");
    let stdout = timed.replace_all(stdout, r#""$1": "<time>""#);
    // `-vv` opens with a block of the machine's paths in the reference and with Volant's own
    // version line here: the generator writes the same placeholder for the block.
    let version = regex::Regex::new(r"(?m)^volant .* \(ansible-core [0-9.]+ compatible\)\n")
        .expect("the pattern compiles");
    let stdout = version.replace_all(&stdout, "<golden-version>\n");
    // A job's id, `j<random>.<pid>`, and the process its limit killed: drawn anew by every run.
    let jid = regex::Regex::new(r"\bj[0-9]+\.[0-9]+\b").expect("the pattern compiles");
    let stdout = jid.replace_all(&stdout, "<jid>");
    let pid = regex::Regex::new(r#""child_pid": [0-9]+"#).expect("the pattern compiles");
    let stdout = pid.replace_all(&stdout, r#""child_pid": <pid>"#);
    // A controller temp file named in a diff header, drawn anew by every run: the recording
    // already reads `<golden-ansible-tmp>` there. Volant's decrypted vaulted source sits in
    // a private directory of its own.
    let stdout = regex::Regex::new(r"\S*/\.ansible/tmp/ansible-local-\w+/tmp\w+|\S*/\.tmp\w+/\S+")
        .expect("the pattern compiles")
        .replace_all(&stdout, "<golden-ansible-tmp>")
        .into_owned();
    if unordered.is_empty() {
        return stdout;
    }
    let lines: Vec<&str> = stdout.split_inclusive('\n').collect();
    let loose = |line: &str| {
        unordered
            .iter()
            .any(|re| re.is_match(line.trim_end_matches('\n')))
    };
    let mut moved: Vec<&str> = lines.iter().copied().filter(|line| loose(line)).collect();
    moved.sort_unstable();
    let mut moved = moved.into_iter();
    lines
        .iter()
        .map(|line| {
            if loose(line) {
                moved.next().expect("as many lines")
            } else {
                line
            }
        })
        .collect()
}

/// A copy of `from` under `to`, links kept as links.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("the copy is writable");
    for entry in std::fs::read_dir(from).expect("the scenario lists") {
        let entry = entry.expect("the scenario lists");
        let target = to.join(entry.file_name());
        let kind = entry.file_type().expect("an entry has a type");
        if kind.is_dir() {
            copy_dir(&entry.path(), &target);
        } else if kind.is_symlink() {
            let link = std::fs::read_link(entry.path()).expect("the link reads");
            #[cfg(unix)]
            std::os::unix::fs::symlink(link, &target).expect("the link copies");
            #[cfg(not(unix))]
            panic!("{}: a link in a scenario needs unix", link.display());
        } else {
            std::fs::copy(entry.path(), &target).expect("the file copies");
        }
    }
}

/// One scenario under `tests/scenarios`: each run of Volant, in a fresh copy of the directory, has
/// the exit code and the whole stdout ansible-core printed there.
#[rstest]
fn a_scenario_matches_the_reference(#[files("tests/scenarios/*/case.yml")] case: PathBuf) {
    let dir = case.parent().expect("a case sits in its scenario");
    let name = dir
        .file_name()
        .expect("a scenario has a name")
        .to_string_lossy()
        .into_owned();
    let text = std::fs::read_to_string(&case).expect("case.yml is readable");
    let docs = yaml::load(&text, &case.display().to_string()).expect("case.yml parses");
    let doc = yaml::to_json(&docs[0]).expect("case.yml is JSON-shaped");
    let spec = Case::parse(&name, &doc);
    let (unmet, python) = spec.unmet(&name);
    if !unmet.is_empty() {
        let why = format!("{name} needs {}", unmet.join(", "));
        assert!(std::env::var_os("VOLANT_PYTHON").is_none(), "{why}");
        eprintln!("skipped: {why}");
        return;
    }
    assert!(
        spec.unordered.is_empty() || spec.forks > 1,
        "{name}: `unordered` excuses nothing under `forks: 1`"
    );
    let unordered: Vec<regex::Regex> = spec
        .unordered
        .iter()
        .map(|re| regex::Regex::new(re).unwrap_or_else(|e| panic!("{name}: {re}: {e}")))
        .collect();
    let recorded: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("expected.json")).expect("expected.json is readable"),
    )
    .expect("expected.json parses");
    let expected: Vec<&Value> = if spec.single {
        vec![&recorded]
    } else {
        recorded
            .as_array()
            .expect("expected.json of `runs` is a list")
            .iter()
            .collect()
    };
    let runs = &spec.runs;
    assert_eq!(
        runs.len(),
        expected.len(),
        "{name}: case.yml and expected.json hold as many runs"
    );
    let empty_config =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/reference/ansible.cfg");
    for (i, (args, want)) in runs.iter().zip(expected).enumerate() {
        let copy =
            std::env::temp_dir().join(format!("volant-scenario-{}-{name}-{i}", std::process::id()));
        let _ = std::fs::remove_dir_all(&copy);
        copy_dir(dir, &copy);
        let own = copy.join("ansible.cfg");
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_volant"));
        command
            .arg("playbook")
            .args(args)
            .args(["--forks", &spec.forks.to_string()])
            .current_dir(&copy)
            .env_remove("COLUMNS");
        // Except where the controller's collections are installed: the helper runs a
        // collection's filter from there, as the reference did when it was recorded.
        for (var, _) in std::env::vars() {
            if (var.starts_with("ANSIBLE_") || var.starts_with("VOLANT_"))
                && var != "ANSIBLE_COLLECTIONS_PATH"
            {
                command.env_remove(var);
            }
        }
        command
            .env(
                "ANSIBLE_CONFIG",
                if own.exists() {
                    own
                } else {
                    empty_config.clone()
                },
            )
            .env("ANSIBLE_NOCOLOR", "1")
            .env("NO_COLOR", "1")
            .envs(spec.env.iter().map(|(k, v)| (k, v)));
        if let Some(python) = &python {
            command.env("VOLANT_PYTHON", python);
        }
        let out = process::run_within(&mut command, DEADLINE, None);
        // macOS resolves its temporary directory under `/private`: mask both spellings.
        let canonical = copy.canonicalize().unwrap_or_else(|_| copy.clone());
        let _ = std::fs::remove_dir_all(&copy);
        // The generator reads the reference's stdout in Python's text mode, where `\r\n` and a
        // lone `\r` both read as `\n` (a `pause` prompt ends in `\r`): read this one the same way.
        let got = String::from_utf8_lossy(&out.stdout)
            .replace(&canonical.display().to_string(), "<golden-tmp>")
            .replace(&copy.display().to_string(), "<golden-tmp>")
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let shown = format!(
            "{name}: {}\n--- stderr\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        let code = want["code"].as_i64().and_then(|c| i32::try_from(c).ok());
        let wanted = want["stdout"].as_str().expect("a recorded stdout");
        assert_eq!(
            out.status.code(),
            code,
            "exit code of {shown}--- stdout\n{got}"
        );
        assert_eq!(
            normalise(&got, &unordered),
            normalise(wanted, &unordered),
            "stdout of {shown}"
        );
    }
}

/// Every case of `inventories/cases.yml` read as `ansible-inventory --list` read it: the exit
/// code, the warnings and errors, the dump, and the host order `--list-hosts all` printed.
#[test]
fn inventory_sources_match_the_reference() {
    use volant::inventory::{Note, PluginConfig, Source, load_sources};
    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let expected: Value =
        serde_json::from_str(include_str!("golden/expected_inventory.json")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("inventories");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/inventories"),
        &root,
    );
    std::fs::create_dir(root.join("empty")).unwrap();
    let shown_root = root.to_string_lossy().into_owned();
    let own_path = std::env::var("PATH").unwrap_or_default();
    let mut failures = Vec::new();
    let cases = expected["sources"].as_object().unwrap();
    assert!(!cases.is_empty(), "no recorded inventory source case");
    for (name, case) in cases {
        let sources: Vec<Source> = case["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .map(|s| {
                if s.contains(',') {
                    Source::new(s)
                } else {
                    Source::new(&root.join(s).to_string_lossy())
                }
            })
            .collect();
        let env = &case["env"];
        // A collection plugin runs in ansible-core's own Python: no recorded one, no case.
        if case["needs"].as_array().is_some_and(|n| !n.is_empty()) {
            let Some(python) = python::reference_python() else {
                continue;
            };
            // Safety: nextest runs each test in its own process; nothing else reads it here.
            unsafe { std::env::set_var("VOLANT_PYTHON", python) };
        }
        if let Some(path) = env.get("PATH").and_then(Value::as_str) {
            // Safety: nextest runs each test in its own process; nothing else reads it here.
            unsafe {
                std::env::set_var(
                    "PATH",
                    path.replace("$COPY", &shown_root)
                        .replace("$PATH", &own_path),
                );
            }
        }
        let config = PluginConfig {
            unparsed_is_failed: env.get("ANSIBLE_INVENTORY_UNPARSED_FAILED").is_some(),
            any_unparsed_is_failed: env
                .get("ANSIBLE_INVENTORY_ANY_UNPARSED_IS_FAILED")
                .is_some(),
            enabled: env
                .get("ANSIBLE_INVENTORY_ENABLED")
                .and_then(|v| v.as_str())
                .map_or_else(
                    || PluginConfig::default().enabled,
                    |v| v.split(',').map(String::from).collect(),
                ),
            ..PluginConfig::default()
        };
        let loaded = load_sources(&sources, &config);
        let mut stderr = String::new();
        let mut seen = std::collections::HashSet::new();
        for note in &loaded.notes {
            let (label, text) = match note {
                Note::Warning(text) => ("[WARNING]: ", text),
                Note::Deprecation(text) => {
                    if !stderr.contains("Deprecation warnings can be") {
                        stderr.push_str(
                            "[WARNING]: Deprecation warnings can be disabled by setting \
                             `deprecation_warnings=False` in ansible.cfg.\n",
                        );
                    }
                    ("[DEPRECATION WARNING]: ", text)
                }
                Note::Verbose(..) => continue,
            };
            if seen.insert(text.clone()) {
                stderr.push_str(&[label, text, "\n"].concat());
            }
        }
        // What looking the hosts up warns, as `ansible-inventory` does after reading the sources.
        for warning in loaded.inventory.resolve("all").warnings {
            if seen.insert(warning.clone()) {
                stderr.push_str(&["[WARNING]: ", warning.as_str(), "\n"].concat());
            }
        }
        if let Some(fatal) = &loaded.error {
            stderr.push_str(
                &[
                    "[ERROR]: ",
                    fatal.message.as_str(),
                    "
",
                ]
                .concat(),
            );
        }
        let stderr = stderr.replace(&shown_root, "<golden-tmp>");
        let rc = loaded.error.as_ref().map_or(0, |f| f.code);
        if case["rc"] != rc {
            failures.push(format!("{name}: exit {}, ours {rc}", case["rc"]));
        }
        if stderr != case["stderr"].as_str().unwrap() {
            failures.push(format!(
                "{name}: stderr
--- reference
{}--- ours
{stderr}",
                case["stderr"].as_str().unwrap()
            ));
        }
        if !case["list"].is_null() && loaded.inventory.dump() != case["list"] {
            failures.push(format!(
                "{name}: --list
reference {}
ours      {}",
                case["list"],
                loaded.inventory.dump()
            ));
        }
        let hosts: Vec<String> = loaded
            .inventory
            .resolve("all")
            .hosts
            .into_iter()
            .map(|h| h.name)
            .collect();
        if rc == 0 && Value::from(hosts.clone()) != case["hosts"] {
            failures.push(format!("{name}: hosts {}, ours {hosts:?}", case["hosts"]));
        }
    }
    assert!(
        failures.is_empty(),
        "{}",
        failures.join(
            "
"
        )
    );
}

/// `inventory.ini`'s hosts, variables and groups, and every pattern of `patterns.txt`, resolved
/// as `ansible-inventory` and `ansible --list-hosts` did.
#[test]
fn inventory_matches_the_reference() {
    let expected: Value =
        serde_json::from_str(include_str!("golden/expected_inventory.json")).unwrap();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/inventory.ini");
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
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/homonym_inventory.ini");
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

/// Every recording in `module/` is indexed and run by the recorded play, and every native candidate
/// and both `cron` cases have an indexed case.
#[cfg(target_os = "linux")]
fn every_recording_is_indexed() {
    let index = module_file("index.json");
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
    let play = module_file("play.json");
    let run: std::collections::BTreeSet<&str> = play_tasks(&play)
        .into_iter()
        .filter_map(|task| task["name"].as_str())
        .collect();
    let recorded: std::collections::BTreeSet<String> = std::fs::read_dir(MODULE_GOLDEN)
        .expect("module/ is readable")
        .map(|entry| {
            entry
                .expect("module/ lists")
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
                "module/{file}: every recorded case is indexed, and index.json does not name this \
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

/// A `setup` handed back for a host outside the native's subset is said and passes; handed back
/// for anything else, it is red. Checked on made-up profile lines: the machines these run on
/// answer, except the CI runner, which has `/usr/bin/rpm`.
///
#[cfg(target_os = "linux")]
fn only_a_host_outside_the_native_setup_excuses_its_hand_back() {
    let spec = serde_json::json!({"module": "setup", "expect": "native"});
    let judge = |module: &str, reason: &str| {
        let profile = NativeProfile {
            natives: Some(vec!["setup".into(), "stat".into()]),
            tasks: vec![serde_json::json!({
                "host": "localhost", "task": "c", "module": module, "path": "fallback", "reason": reason,
            })],
        };
        let mut spec = spec.clone();
        spec["module"] = module.into();
        let mut failures = Vec::new();
        check_native_path("c", &spec, &profile, &mut failures);
        failures
    };
    let host = "/usr/bin/rpm exists, and the reference asks it about apt-get";
    assert_eq!(judge("setup", host), Vec::<String>::new());
    assert_eq!(
        judge("setup", "a filter is outside the native setup"),
        vec!["case c: path fallback (a filter is outside the native setup), index says native"]
    );
    assert_eq!(
        judge("stat", host),
        vec![format!("case c: path fallback ({host}), index says native")]
    );
}

/// A `package_facts` handed back because the host's apt sources use another scheme (the CI
/// runner's `mirror+file:`) is said and passes; handed back for anything else, it is red, so a
/// host whose sources are all http(s) still has to be answered natively.
#[cfg(target_os = "linux")]
fn only_a_source_scheme_excuses_a_package_facts_hand_back() {
    let judge = |module: &str, reason: &str| {
        let profile = NativeProfile {
            natives: Some(vec!["package_facts".into(), "stat".into()]),
            tasks: vec![serde_json::json!({
                "host": "localhost", "task": "c", "module": module, "path": "fallback", "reason": reason,
            })],
        };
        let spec = serde_json::json!({"module": module, "expect": "native"});
        let mut failures = Vec::new();
        check_native_path("c", &spec, &profile, &mut failures);
        failures
    };
    for scheme in [
        "the source scheme mirror+file is not http or https",
        "the source scheme mirror is not http or https",
    ] {
        assert_eq!(judge("package_facts", scheme), Vec::<String>::new());
        assert_eq!(
            judge("stat", scheme),
            vec![format!(
                "case c: path fallback ({scheme}), index says native"
            )]
        );
    }
    for other in [
        "a source whose scheme is not http or https",
        "the source scheme  is not http or https",
        "x_Packages is a list no configured source names",
        "python3-apt is not installed for /usr/bin/python3",
        "a source URI is not a plain one",
    ] {
        assert_eq!(
            judge("package_facts", other),
            vec![format!(
                "case c: path fallback ({other}), index says native"
            )]
        );
    }
}

/// The module index against its recordings, its play and the native candidates, and the two
/// hand-backs it excuses.
#[cfg(target_os = "linux")]
#[rstest]
#[case::every_recording_is_indexed(every_recording_is_indexed)]
#[case::only_a_host_outside_the_native_setup_excuses_its_hand_back(
    only_a_host_outside_the_native_setup_excuses_its_hand_back
)]
#[case::only_a_source_scheme_excuses_a_package_facts_hand_back(
    only_a_source_scheme_excuses_a_package_facts_hand_back
)]
fn the_module_index_holds(#[case] check: fn()) {
    check();
}
