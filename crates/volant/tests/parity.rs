// SPDX-License-Identifier: GPL-3.0-or-later
//! The parity matrix: every name ansible-core 2.19.12 and the collection corpus know, against
//! what this release does with it. `parity/reference.json` is the reference's side, written by
//! `just parity-reference`; everything else is asked of the engine.

use std::fmt::Write as _;

use clap::Parser as _;
use serde_json::Value;
use volant::bridge::PluginKind;
use volant::keywords::{self, Support};
use volant_protocol::modules;

fn reference() -> Value {
    serde_json::from_str(include_str!("parity/reference.json")).expect("reference.json")
}

fn names(v: &Value, path: &[&str]) -> Vec<String> {
    let mut at = v;
    for p in path {
        at = &at[*p];
    }
    at.as_array()
        .unwrap_or_else(|| panic!("{path:?} is not a list in reference.json"))
        .iter()
        .map(|n| n.as_str().unwrap().to_string())
        .collect()
}

/// A place keywords are written, its table, and whether a name has a row there.
type Place = (&'static str, &'static [keywords::Keyword], fn(&str) -> bool);

/// Every keyword the reference knows has a row in `keywords.rs`, at the same place, and no row
/// there names a keyword the reference does not know.
#[test]
fn the_keyword_tables_are_the_references() {
    let r = reference();
    let places: [Place; 4] = [
        ("play", keywords::PLAY_KEYWORDS, |n| {
            keywords::play_keyword(n).is_some()
        }),
        ("block", keywords::BLOCK_KEYWORDS, |n| {
            keywords::block_keyword(n).is_some()
        }),
        ("task", keywords::TASK_KEYWORDS, |n| {
            keywords::task_keyword(n).is_some()
        }),
        ("loop_control", keywords::LOOP_CONTROL_KEYWORDS, |n| {
            keywords::loop_control_keyword(n).is_some()
        }),
    ];
    for (place, table, known) in places {
        let theirs = names(&r, &["keywords", place]);
        for n in &theirs {
            assert!(
                known(n),
                "{place} keyword '{n}' is missing from keywords.rs"
            );
        }
        for k in table {
            assert!(
                theirs.contains(&k.name.to_string()),
                "{place} keyword '{}' is not the reference's",
                k.name
            );
        }
    }
    for n in names(&r, &["keywords", "handler"]) {
        assert!(
            keywords::handler_keyword(&n).is_some() || keywords::task_keyword(&n).is_some(),
            "handler keyword '{n}' is missing from keywords.rs"
        );
    }
}

/// Every `meta` action the reference accepts is either run or refused by name.
#[test]
fn every_meta_action_is_known() {
    for action in names(&reference(), &["meta"]) {
        let text = format!("- hosts: all\n  gather_facts: false\n  tasks:\n    - meta: {action}\n");
        let pb = volant::playbook::parse(&text, "parity.yml").expect("parses");
        if let Err(e) = volant::preflight::check(&pb) {
            let msg = format!("{e:#}");
            assert!(
                msg.contains("is not supported yet"),
                "meta '{action}': {msg}"
            );
        }
    }
}

/// Every builtin module the reference ships is a builtin here, and has a route.
#[test]
fn the_builtin_modules_are_the_references() {
    let r = reference();
    let theirs = names(&r, &["builtin", "module"]);
    for m in &theirs {
        assert!(
            modules::builtin_route(m).is_some(),
            "builtin module '{m}' has no route"
        );
    }
    for m in modules::BUILTIN_MODULES {
        assert!(
            theirs.contains(&m.to_string()),
            "'{m}' is not a builtin module of the reference"
        );
    }
    let actions = names(&r, &["builtin", "action"]);
    for (m, _) in modules::ACTION_PLUGINS {
        assert!(
            actions.contains(&m.to_string()),
            "'{m}' has no action plugin in the reference"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Runs,
    Partial,
    NotYet,
    Later,
}

impl Status {
    fn badge(self) -> &'static str {
        match self {
            Status::Runs => "<span class=\"kw kw-runs\">runs</span>",
            Status::Partial => "<span class=\"kw kw-partial\">partial</span>",
            Status::NotYet => "<span class=\"kw kw-notyet\">not yet</span>",
            Status::Later => "<span class=\"kw kw-no\">later</span>",
        }
    }
}

struct Row {
    category: String,
    name: String,
    status: Status,
    note: String,
}

fn row(category: &str, name: &str, status: Status, note: &str) -> Row {
    Row {
        category: category.into(),
        name: name.into(),
        status,
        note: note.into(),
    }
}

/// Names outside this milestone, with why. Decided here, never on the page.
const LATER: &[(&str, &str, &str)] = &[
    ("become", "runas", "Windows"),
    ("connection", "psrp", "Windows"),
    ("connection", "winrm", "Windows"),
    ("connection", "_paramiko_ssh", "another connection"),
    ("connection", "paramiko_ssh", "another connection"),
    ("shell", "cmd", "Windows"),
    ("shell", "powershell", "Windows"),
];

fn later(category: &str, name: &str) -> Option<&'static str> {
    if category == "callback" && name != "default" {
        return Some("a stdout callback");
    }
    LATER
        .iter()
        .find(|(c, n, _)| *c == category && *n == name)
        .map(|(_, _, why)| *why)
}

fn keyword_status(k: &keywords::Keyword) -> (Status, String) {
    match k.support {
        Support::Runs => (Status::Runs, String::new()),
        Support::Partial => (Status::Partial, k.missing.unwrap_or_default().to_string()),
        Support::Preflight => (Status::NotYet, String::new()),
    }
}

/// What a name the engine has no native implementation of does: it runs in the Python helper,
/// except the few names the helper is never sent.
fn bridged(kind: PluginKind, fqcn: &str) -> (Status, &'static str) {
    if volant::template::bridged_name(kind, fqcn) {
        return (
            Status::Runs,
            "through the Python helper; needs the plugin's Python libraries",
        );
    }
    let why = volant::template::not_sent_reason(kind, fqcn)
        .expect("a name that is not bridged is one the helper is never sent");
    (Status::NotYet, why)
}

/// A filter or test is known when minijinja does not call it unknown. An error about the
/// probe's input (`'x' | combine`) still means the filter exists.
fn jinja_known(t: &volant::template::Templar, expr: &str, unknown: &str) -> bool {
    match t.render(expr, &serde_json::Map::new()) {
        Ok(_) => true,
        Err(e) => !e.0.contains(unknown),
    }
}

fn filter_status(t: &volant::template::Templar, fqcn: &str, short: Option<&str>) -> Status {
    let known = |n: &str| jinja_known(t, &format!("{{{{ 'x' | {n} }}}}"), "unknown filter");
    let both = known(fqcn) && short.is_none_or(known);
    if both {
        Status::Runs
    } else if known(fqcn) || short.is_some_and(known) {
        Status::Partial
    } else {
        Status::NotYet
    }
}

fn test_status(t: &volant::template::Templar, fqcn: &str, short: Option<&str>) -> Status {
    let known = |n: &str| jinja_known(t, &format!("{{{{ 'x' is {n} }}}}"), "unknown test");
    let both = known(fqcn) && short.is_none_or(known);
    if both {
        Status::Runs
    } else if known(fqcn) || short.is_some_and(known) {
        Status::Partial
    } else {
        Status::NotYet
    }
}

fn defaults() -> volant::transport::ConnectionDefaults {
    volant::transport::ConnectionDefaults {
        remote_user: None,
        connection: None,
        private_key: None,
        host_key_checking: false,
        remote_tmp: "~/.ansible/tmp".into(),
        connect_timeout: std::time::Duration::from_secs(10),
        r#become: false,
        become_user: "root".into(),
        become_method: "sudo".into(),
        become_exe: None,
        become_flags: None,
        become_password: None,
        ssh_common_args: Vec::new(),
        ssh_extra_args: Vec::new(),
    }
}

fn matrix(probe_dir: &std::path::Path) -> Vec<Row> {
    let r = reference();
    let t = volant::template::Templar::new(probe_dir.to_path_buf());
    let mut rows = Vec::new();

    for (place, find) in [
        (
            "play",
            keywords::play_keyword as fn(&str) -> Option<&'static keywords::Keyword>,
        ),
        ("block", keywords::block_keyword),
        ("task", keywords::task_keyword),
        ("loop_control", keywords::loop_control_keyword),
    ] {
        for n in names(&r, &["keywords", place]) {
            let (s, note) =
                keyword_status(find(&n).expect("checked by the_keyword_tables_are_the_references"));
            rows.push(row(&format!("keyword ({place})"), &n, s, &note));
        }
    }
    for action in names(&r, &["meta"]) {
        let text = format!("- hosts: all\n  gather_facts: false\n  tasks:\n    - meta: {action}\n");
        let pb = volant::playbook::parse(&text, "parity.yml").unwrap();
        let s = if volant::preflight::check(&pb).is_ok() {
            Status::Runs
        } else {
            Status::NotYet
        };
        rows.push(row("meta action", &action, s, ""));
    }
    for opt in r["cli"].as_array().unwrap() {
        let flags: Vec<&str> = opt["flags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        // Each spelling on its own: Volant may know `-i` and `--inventory` but not the
        // deprecated `--inventory-file`.
        let parses = |flag: &str| {
            let mut argv = vec!["volant", flag];
            if opt["takes_value"].as_bool().unwrap() {
                argv.push("x");
            }
            argv.push("site.yml");
            !matches!(
                volant::cli::PlaybookArgs::try_parse_from(&argv),
                Err(e) if e.kind() == clap::error::ErrorKind::UnknownArgument
            )
        };
        let known: Vec<&str> = flags.iter().copied().filter(|f| parses(f)).collect();
        let (s, note) = if known.len() == flags.len() {
            (Status::Runs, String::new())
        } else if known.is_empty() {
            (Status::NotYet, String::new())
        } else {
            (Status::Partial, format!("only {}", known.join(", ")))
        };
        // Parsed, and without effect: Volant moves files over its agent link, never `scp`
        // or `sftp`.
        let (s, note) = if s == Status::Runs
            && flags
                .iter()
                .any(|f| matches!(*f, "--scp-extra-args" | "--sftp-extra-args"))
        {
            (
                Status::Partial,
                "accepted with no effect: files move over the agent link".to_string(),
            )
        } else {
            (s, note)
        };
        rows.push(row("ansible-playbook option", &flags.join(", "), s, &note));
    }
    for tag in names(&r, &["yaml_tags"]) {
        let text = format!("a: {tag} x\n");
        let docs = volant::yaml::load(&text, "parity.yml").unwrap();
        let s = if volant::yaml::to_json(&docs[0]).is_ok() {
            Status::Runs
        } else {
            Status::NotYet
        };
        rows.push(row("YAML tag", &tag, s, ""));
    }

    for m in names(&r, &["builtin", "module"]) {
        let (s, note) = match modules::builtin_route(&m)
            .expect("checked by the_builtin_modules_are_the_references")
        {
            modules::Route::Agent => (Status::Runs, "agent"),
            modules::Route::Controller => (Status::Runs, "controller"),
            modules::Route::Statement => (Status::Runs, "statement"),
            modules::Route::ActionPlugin => (Status::Runs, "action plugin"),
            modules::Route::NativeOrPython => (Status::Runs, "agent, Python fallback"),
            modules::Route::Python => (Status::Runs, "Python"),
        };
        rows.push(row("module", &m, s, note));
    }
    // Every one runs: `normal` has no module, it is the plain module run.
    for a in names(&r, &["builtin", "action"]) {
        rows.push(row("action plugin", &a, Status::Runs, ""));
    }
    for f in names(&r, &["builtin", "filter"]) {
        let fqcn = format!("ansible.builtin.{f}");
        let (s, note) = match filter_status(&t, &fqcn, Some(&f)) {
            Status::NotYet => bridged(PluginKind::Filter, &fqcn),
            Status::Partial => (Status::Partial, "one spelling only"),
            s => (s, ""),
        };
        rows.push(row("filter", &f, s, note));
    }
    for f in names(&r, &["builtin", "test"]) {
        let fqcn = format!("ansible.builtin.{f}");
        let (s, note) = match test_status(&t, &fqcn, Some(&f)) {
            Status::NotYet => bridged(PluginKind::Test, &fqcn),
            Status::Partial => (Status::Partial, "one spelling only"),
            s => (s, ""),
        };
        rows.push(row("test", &f, s, note));
    }
    for l in names(&r, &["builtin", "lookup"]) {
        let (s, note) = if volant::template::lookup_supported(&l) {
            (Status::Runs, "")
        } else {
            bridged(PluginKind::Lookup, &l)
        };
        rows.push(row("lookup", &l, s, note));
    }
    for (category, runs) in [
        (
            "become",
            (|n: &str| volant::playbook::BECOME_METHODS.contains(&n)) as fn(&str) -> bool,
        ),
        ("strategy", |n: &str| {
            volant::preflight::STRATEGIES.contains(&n)
        }),
        ("inventory", |n: &str| n == "ini"),
        ("shell", |n: &str| n == "sh"),
        ("vars", |n: &str| n == "host_group_vars"),
        ("cache", |n: &str| n == "memory"),
        ("callback", |n: &str| n == "default"),
        ("connection", |n: &str| {
            let vars: serde_json::Map<String, Value> =
                [("ansible_connection".to_string(), Value::from(n))]
                    .into_iter()
                    .collect();
            volant::transport::Transport::for_vars("h", &vars, &defaults()).is_ok()
        }),
    ] {
        for n in names(&r, &["builtin", category]) {
            let (s, note) = match (runs(&n), later(category, &n)) {
                (true, _) => (Status::Runs, String::new()),
                (false, Some(why)) => (Status::Later, why.to_string()),
                (false, None) => (Status::NotYet, String::new()),
            };
            // The strategies the engine runs with something refused by name.
            let (s, note) = match (category, n.as_str(), s) {
                ("strategy", "free", Status::Runs) => {
                    (Status::Partial, "refused with `max_fail_percentage`".to_string())
                }
                ("strategy", "host_pinned", Status::Runs) => (
                    Status::Partial,
                    "refused with `max_fail_percentage`, and with fewer forks than hosts when the play has handlers, dynamic includes, `run_once`, `pause` or `any_errors_fatal`".to_string(),
                ),
                ("strategy", "debug", Status::Runs) => (
                    Status::Partial,
                    "the prompt takes `c` and `q` only".to_string(),
                ),
                _ => (s, note),
            };
            rows.push(row(category, &n, s, &note));
        }
    }

    for c in r["collections"].as_object().unwrap().values() {
        // What a module or action plugin needs beyond ssh or local (Windows, a network
        // device's persistent connection), as the generator read it from the collection.
        let later = |name: &str| c["later"][name].as_str();
        let actions = names(c, &["action"]);
        let mut push = |kind: &str, name: &str, s: Status, note: &str| {
            rows.push(row(&format!("collection {kind}"), name, s, note));
        };
        for m in names(c, &["module"]) {
            // A module that runs through an action plugin runs on the bridge, as that plugin.
            match (later(&m), c["module_action"][&m].as_str()) {
                (Some(why), _) => push("module", &m, Status::Later, why),
                (None, Some(_)) => push("module", &m, Status::Runs, "through its action plugin"),
                (None, None) => push("module", &m, Status::Runs, ""),
            }
        }
        for a in &actions {
            match later(a) {
                Some(why) => push("action plugin", a, Status::Later, why),
                None => push(
                    "action plugin",
                    a,
                    Status::Runs,
                    "through the Python helper",
                ),
            }
        }
        for f in names(c, &["filter"]) {
            let (s, note) = match filter_status(&t, &f, None) {
                Status::NotYet => bridged(PluginKind::Filter, &f),
                s => (s, ""),
            };
            push("filter", &f, s, note);
        }
        for x in names(c, &["test"]) {
            let (s, note) = match test_status(&t, &x, None) {
                Status::NotYet => bridged(PluginKind::Test, &x),
                s => (s, ""),
            };
            push("test", &x, s, note);
        }
        for l in names(c, &["lookup"]) {
            let (s, note) = if volant::template::lookup_supported(&l) {
                (Status::Runs, "")
            } else {
                bridged(PluginKind::Lookup, &l)
            };
            push("lookup", &l, s, note);
        }
    }
    rows
}

const CATEGORIES_IN_FULL: &[&str] = &[
    "keyword (play)",
    "keyword (block)",
    "keyword (task)",
    "keyword (loop_control)",
    "meta action",
    "ansible-playbook option",
    "YAML tag",
    "module",
    "action plugin",
    "filter",
    "test",
    "lookup",
    "become",
    "connection",
    "inventory",
    "strategy",
    "shell",
    "vars",
    "cache",
    "callback",
];

fn page(rows: &[Row], r: &Value) -> String {
    let mut out = format!(
        "---\ntitle: Parity\ndescription: Each keyword and plugin of ansible-core {core} and of the collections in the ansible {pkg} package, with what Volant does with it.\n---\n\n<!-- Generated by tests/parity.rs from tests/parity/reference.json by `just docs-parity`. Do not edit by hand. -->\n\nEach row is a keyword, option or plugin that ansible-core {core} ships, or a module, filter, test, lookup or action plugin from a collection in the ansible {pkg} package, with what this release of Volant does with it. `later` marks the Windows parts, the other connection types and the stdout callbacks, which come after the rest.\n\n## Summary\n\n| Category | runs | partial | not yet | later |\n|---|---|---|---|---|\n",
        core = r["ansible_core"].as_str().unwrap(),
        pkg = r["ansible_package"].as_str().unwrap(),
    );
    let mut seen: Vec<&str> = Vec::new();
    for x in rows {
        if !seen.contains(&x.category.as_str()) {
            seen.push(&x.category);
        }
    }
    for c in seen {
        let count = |s: Status| {
            rows.iter()
                .filter(|x| x.category == c && x.status == s)
                .count()
        };
        let _ = writeln!(
            out,
            "| {c} | {} | {} | {} | {} |",
            count(Status::Runs),
            count(Status::Partial),
            count(Status::NotYet),
            count(Status::Later)
        );
    }
    for c in CATEGORIES_IN_FULL {
        let _ = write!(out, "\n## {c}\n\n| Name | Status | Note |\n|---|---|---|\n");
        for x in rows.iter().filter(|x| x.category == *c) {
            let _ = writeln!(out, "| `{}` | {} | {} |", x.name, x.status.badge(), x.note);
        }
    }
    out.push_str("\n## Collections\n\nFor each collection, how many of its rows run, out of the total.\n\n| Collection | Version | modules | filters | tests | lookups | action plugins |\n|---|---|---|---|---|---|---|\n");
    for (fqcn, c) in r["collections"].as_object().unwrap() {
        let prefix = format!("{fqcn}.");
        let cell = |kind: &str| {
            let mine: Vec<&Row> = rows
                .iter()
                .filter(|x| {
                    x.category == format!("collection {kind}") && x.name.starts_with(&prefix)
                })
                .collect();
            let runs = mine.iter().filter(|x| x.status == Status::Runs).count();
            if mine.iter().all(|x| x.status == Status::Later) && !mine.is_empty() {
                "later".to_string()
            } else {
                format!("{runs}/{}", mine.len())
            }
        };
        let _ = writeln!(
            out,
            "| `{fqcn}` | {} | {} | {} | {} | {} | {} |",
            c["version"].as_str().unwrap(),
            cell("module"),
            cell("filter"),
            cell("test"),
            cell("lookup"),
            cell("action plugin")
        );
    }
    out
}

/// An empty directory of its own for one test's probes.
fn probe_dir(test: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("volant-parity-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The parity page lists exactly what the engine supports, as `just docs-parity` writes it.
#[test]
fn the_parity_page_matches_the_engine() {
    // The lookup rows ask `lookup_supported`, a name check that never runs a lookup.
    let rows = matrix(&probe_dir("page"));
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/src/content/docs/reference/parity.md");
    let expected = page(&rows, &reference());
    if std::env::var_os("VOLANT_UPDATE_DOCS").is_some() {
        std::fs::write(&path, &expected).unwrap();
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        actual, expected,
        "run `just docs-parity` to regenerate docs/src/content/docs/reference/parity.md"
    );
}

/// The probes tell an existing filter that rejects its input from a missing one, and a
/// collection plugin is never taken for the builtin of the same short name.
#[test]
fn the_probes_classify_what_they_should() {
    let t = volant::template::Templar::new(probe_dir("probes"));
    assert_eq!(
        filter_status(&t, "ansible.builtin.combine", Some("combine")),
        Status::Runs
    );
    assert_eq!(
        filter_status(&t, "ansible.builtin.nosuch_filter", Some("nosuch_filter")),
        Status::NotYet
    );
    assert_eq!(
        filter_status(&t, "community.general.to_json", None),
        Status::NotYet
    );
    assert_eq!(
        test_status(&t, "ansible.builtin.defined", Some("defined")),
        Status::Runs
    );
    assert!(!volant::template::lookup_supported(
        "community.general.pipe"
    ));
}

/// A Windows or network collection holds its modules and action plugins for later, while its
/// filters, tests and lookups stay in scope.
#[test]
fn a_windows_or_network_collection_waits_only_for_its_modules_and_actions() {
    let rows = matrix(&probe_dir("traits"));
    let of = |category: &str, prefix: &str| -> Vec<Status> {
        rows.iter()
            .filter(|x| x.category == category && x.name.starts_with(prefix))
            .map(|x| x.status)
            .collect()
    };
    let windows_modules = of("collection module", "ansible.windows.");
    assert!(!windows_modules.is_empty());
    assert!(
        windows_modules.iter().all(|s| *s == Status::Later),
        "{windows_modules:?}"
    );
    let network_filters = of("collection filter", "ansible.netcommon.");
    assert!(!network_filters.is_empty());
    assert!(
        network_filters.iter().all(|s| *s != Status::Later),
        "{network_filters:?}"
    );
}

/// An option runs when every spelling the reference accepts parses here, and is partial when only
/// some do.
#[test]
fn an_option_is_judged_on_every_spelling() {
    let rows = matrix(&probe_dir("options"));
    let status = |name: &str| {
        rows.iter()
            .find(|x| x.category == "ansible-playbook option" && x.name == name)
            .map(|x| x.status)
    };
    assert_eq!(
        status("-i, --inventory, --inventory-file"),
        Some(Status::Runs)
    );
    assert_eq!(status("-C, --check"), Some(Status::Runs));
}

/// A collection's module waits for a later release only when it needs another platform: a network
/// device's persistent connection, or Windows.
#[test]
fn a_module_waits_only_when_it_needs_another_platform() {
    let rows = matrix(&probe_dir("platform"));
    let status = |name: &str| {
        rows.iter()
            .find(|x| x.category == "collection module" && x.name == name)
            .map(|x| x.status)
    };
    assert_eq!(
        status("community.vmware.vcenter_extension"),
        Some(Status::Runs)
    );
    assert_eq!(status("cisco.ios.ios_acls"), Some(Status::Later));
}

/// Every builtin action plugin of the reference is implemented here or refused by name: a new one
/// would otherwise run as its plain module and be counted as running.
#[test]
fn every_builtin_action_plugin_is_implemented_or_refused() {
    for a in names(&reference(), &["builtin", "action"]) {
        if a == "normal" {
            continue; // the plain module run, which every module goes through
        }
        assert!(
            matches!(
                modules::builtin_route(&a),
                Some(
                    modules::Route::Agent
                        | modules::Route::Controller
                        | modules::Route::ActionPlugin
                )
            ),
            "action plugin '{a}' is neither implemented nor refused"
        );
    }
}

/// The engine refuses by name the collection plugins and modules this page marks `later`, from a
/// list written out of the same reference data.
#[test]
fn the_names_the_engine_refuses_are_the_ones_marked_later() {
    let mut waiting = std::collections::BTreeSet::new();
    for c in reference()["collections"].as_object().unwrap().values() {
        let held = |name: &str| {
            !c["module_action"][name].is_null()
                || c["action"].as_array().unwrap().iter().any(|a| a == name)
        };
        for name in c["later"].as_object().unwrap().keys().filter(|n| held(n)) {
            waiting.insert(name.clone());
        }
    }
    let written: Vec<&str> = include_str!("../src/later_actions.txt").lines().collect();
    assert_eq!(
        written,
        waiting.iter().map(String::as_str).collect::<Vec<_>>()
    );
}
