// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::Path;

use super::facts::restricted_fact;
use super::load::extend;
use super::*;
use crate::template::Vars;
use crate::testing::{scratch, write};
use serde_json::json;

fn tree() -> tempfile::TempDir {
    let tmp = scratch();
    let dir = tmp.path();
    for (rel, text) in [
        (
            "inventory/hosts.ini",
            "[web]\nweb1 tier=ini\n[web:vars]\nfrom=ini_group\n",
        ),
        (
            "inventory/group_vars/all.yml",
            "layer: inv_all\nonly_inv_all: 1\n",
        ),
        (
            "inventory/group_vars/web/10-first.yml",
            "layer: inv_web\nsplit: first\n",
        ),
        ("inventory/group_vars/web/20-second.yaml", "split: second\n"),
        ("inventory/host_vars/web1.json", r#"{"layer": "inv_host"}"#),
        ("play/group_vars/all", "play_all: yes\n"),
        ("play/host_vars/web1.yml", "layer: play_host\n"),
        ("play/group_vars/.hidden.yml", "hidden: true\n"),
        ("play/group_vars/all.yml~", "backup: true\n"),
    ] {
        write(dir, rel, text);
    }
    tmp
}

fn store(dir: &Path) -> (Inventory, VarStore) {
    let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
    let store = VarStore::new(
        &inv,
        Some(&dir.join("inventory/hosts.ini")),
        &dir.join("play"),
        Map::new(),
    )
    .unwrap();
    (inv, store)
}

fn scope(hosts: &[&str]) -> Scope {
    let names: Vec<String> = hosts.iter().map(ToString::to_string).collect();
    Scope {
        play_hosts: names.clone(),
        batch_hosts: names.clone(),
        all_play_hosts: names,
        ..Scope::default()
    }
}

/// Directories layer in name order under the play, vars_files, task vars, `set_fact` and `-e`;
/// hidden and backup files are ignored.
#[test]
fn directory_and_run_layers_stack_in_the_reference_order() {
    {
        let tmp = tree();
        let dir = tmp.path();
        let (_, mut store) = store(dir);
        let v = store.for_host("web1", &scope(&["web1"]));
        assert_eq!(
            v["layer"],
            json!("play_host"),
            "playbook host_vars is the highest directory source"
        );
        assert_eq!(v["only_inv_all"], json!(1));
        assert_eq!(
            v["split"],
            json!("second"),
            "files in a directory apply in name order"
        );
        assert_eq!(v["from"], json!("ini_group"));
        assert_eq!(v["tier"], json!("ini"));
        assert_eq!(
            v["play_all"],
            json!(true),
            "yes as data is a boolean too, matching PyYAML"
        );
        assert!(!v.contains_key("hidden") && !v.contains_key("backup"));
    }
    {
        let tmp = tree();
        let dir = tmp.path();
        let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
        let extra: Map<String, Value> = json!({"layer": "extra"}).as_object().unwrap().clone();
        let mut store = VarStore::new(
            &inv,
            Some(&dir.join("inventory/hosts.ini")),
            &dir.join("play"),
            extra,
        )
        .unwrap();
        let mut sc = scope(&["web1"]);
        sc.play_vars = json!({"layer": "play", "p": 1})
            .as_object()
            .unwrap()
            .clone();
        sc.vars_files = vec![json!({"p": 2, "f": 1}).as_object().unwrap().clone()];
        sc.task_vars = json!({"f": 2, "t": 1}).as_object().unwrap().clone();
        store.set_fact("web1", "t", json!(2));
        store.set_fact("web1", "fact", json!("set"));
        let v = store.for_host("web1", &sc);
        assert_eq!(v["layer"], json!("extra"), "extra vars beat everything");
        assert_eq!(v["p"], json!(2), "vars_files beat play vars");
        assert_eq!(v["f"], json!(2), "task vars beat vars_files");
        assert_eq!(v["t"], json!(2), "set_fact beats task vars");
        assert_eq!(v["fact"], json!("set"));
    }
}

/// Every host variable the engine reads to decide where, as whom or under what a task runs is
/// one a managed host cannot set through `ansible_facts`.
#[test]
fn every_host_setting_is_stripped_from_gathered_facts() {
    let reader = regex::Regex::new(
        r#"\b(?:host_setting|setting_text|setting_args)\((?:[^;"]*?,)?\s*"(ansible_[a-z0-9_]+)""#,
    )
    .unwrap();
    // A lookup or an index, not an array literal: `vars["ansible_host"]`, never `= ["..."]`.
    let bypass = regex::Regex::new(r#"(?:\.get\(|[\w)\]]\[)"(ansible_[a-z0-9_]+)""#).unwrap();
    let tests = regex::Regex::new(r"#\[cfg\(test\)\]\s*mod (?:tests|testing)\b").unwrap();
    let mut read = BTreeSet::new();
    let mut bypasses = Vec::new();
    let mut stack = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs")
                || path
                    .file_name()
                    .is_some_and(|n| n == "tests.rs" || n == "test_support.rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let code = tests.split(&source).next().unwrap_or_default();
            for c in reader.captures_iter(code) {
                read.insert(c[1].to_string());
            }
            for c in bypass.captures_iter(code) {
                // `template` only asks whether `ansible_managed` is there before writing its
                // own; the value decides nothing and reaches the file as text.
                if !matches!(&c[1], "ansible_facts" | "ansible_managed") {
                    bypasses.push(format!("{}: {}", path.display(), &c[1]));
                }
            }
        }
    }
    assert!(
        bypasses.is_empty(),
        "read by name outside `host_setting`: {bypasses:?}"
    );
    for known in [
        "ansible_host",
        "ansible_become_password",
        "ansible_python_interpreter",
    ] {
        assert!(read.contains(known), "the scan missed {known}: {read:?}");
    }
    let kept: Vec<_> = read.iter().filter(|n| !restricted_fact(n)).collect();
    assert!(
        kept.is_empty(),
        "a managed host can set these for itself: {kept:?}"
    );
}

/// Gathered facts rank under the play and over the inventory; a role's defaults, vars and
/// parameters sit where the reference puts them.
#[test]
#[expect(clippy::too_many_lines)]
fn facts_and_role_layers_rank_as_the_reference_ranks_them() {
    {
        let inv = Inventory::parse_ini("h1 ansible_inv=from-inventory\n").unwrap();
        let mut store = VarStore::new(&inv, None, Path::new("."), Map::new()).unwrap();
        store.set_fact("h1", "ansible_kept", json!("from-set-fact"));
        let facts = json!({"ansible_inv": "Ubuntu", "ansible_play": "Ubuntu", "ansible_task": "Ubuntu", "ansible_kept": "Ubuntu"});
        store.gather_facts("h1", facts.as_object().unwrap());
        let mut sc = scope(&["h1"]);
        sc.play_vars = json!({"ansible_play": "from-play"})
            .as_object()
            .unwrap()
            .clone();
        sc.task_vars = json!({"ansible_task": "from-task"})
            .as_object()
            .unwrap()
            .clone();
        let v = store.for_host("h1", &sc);
        assert_eq!(
            v["ansible_inv"],
            json!("Ubuntu"),
            "a fact beats an inventory variable"
        );
        assert_eq!(
            v["ansible_play"],
            json!("from-play"),
            "a play variable beats a fact"
        );
        assert_eq!(
            v["ansible_task"],
            json!("from-task"),
            "a task variable beats a fact"
        );
        assert_eq!(
            v["ansible_kept"],
            json!("from-set-fact"),
            "a later setup keeps a set_fact"
        );
        let untrusted = store.untrusted_of("h1", &sc);
        assert!(
            untrusted.contains("ansible_inv"),
            "the fact that answers is untrusted"
        );
        for name in ["ansible_play", "ansible_task", "ansible_kept"] {
            assert!(
                !untrusted.contains(name),
                "{name} answers with the author's value"
            );
        }
        assert_eq!(
            store.hostvars_shared("h1")["h1"]["ansible_kept"],
            json!("from-set-fact"),
            "hostvars ranks them the same way"
        );
    }
    {
        let inv = Inventory::parse_ini("h1 d=inv-host\n").unwrap();
        let role = |extra: Map<String, Value>| Scope {
            role_defaults: json!({"d": "role-default", "shared": "role-default"})
                .as_object()
                .unwrap()
                .clone(),
            role_vars: json!({"v": "role-var", "shared": "role-var"})
                .as_object()
                .unwrap()
                .clone(),
            role_params: json!({"p": "param"}).as_object().unwrap().clone(),
            play_vars: json!({"shared": "play-var"}).as_object().unwrap().clone(),
            task_vars: extra,
            ..Scope::default()
        };

        let mut store = VarStore::new(&inv, None, Path::new("."), Map::new()).unwrap();
        store.set_fact("h1", "p", json!("fact"));
        let v = store.for_host("h1", &role(Map::new()));
        assert_eq!(
            v["d"],
            json!("inv-host"),
            "a role default loses to an inventory variable"
        );
        assert_eq!(
            v["shared"],
            json!("role-var"),
            "a role's vars beat the play's own vars:"
        );
        assert_eq!(
            v["p"],
            json!("param"),
            "a role parameter beats a fact of the same name"
        );

        let task = json!({"shared": "task-var"}).as_object().unwrap().clone();
        let v = store.for_host("h1", &role(task));
        assert_eq!(
            v["shared"],
            json!("task-var"),
            "a task's vars: beat a role's vars"
        );
        assert_eq!(
            v["v"],
            json!("role-var"),
            "and leave the rest of them alone"
        );

        let extra: Map<String, Value> = json!({"p": "extra"}).as_object().unwrap().clone();
        let mut store = VarStore::new(&inv, None, Path::new("."), extra).unwrap();
        let v = store.for_host("h1", &role(Map::new()));
        assert_eq!(
            v["p"],
            json!("extra"),
            "an extra var beats a role parameter"
        );
    }
}

/// Magic variables are present, and the batch, live play and resolved play are three host lists.
#[test]
fn the_magic_variables_follow_the_batch_and_the_play() {
    {
        let tmp = tree();
        let dir = tmp.path();
        let (_, mut store) = store(dir);
        let v = store.for_host("web1", &scope(&["web1"]));
        assert_eq!(v["inventory_hostname"], json!("web1"));
        assert_eq!(v["inventory_hostname_short"], json!("web1"));
        assert_eq!(v["group_names"], json!(["web"]));
        for name in [
            "hostvars",
            "groups",
            "ansible_play_hosts",
            "ansible_play_hosts_all",
            "ansible_play_batch",
            "play_hosts",
        ] {
            assert!(
                v.get(name).is_none(),
                "{name} is handed to templates as a shared view, not merged into the map"
            );
        }
        let shared = store.shared_values(&scope(&["web1"]));
        assert_eq!(shared["groups"]["web"], json!(["web1"]));
        assert_eq!(shared["ansible_play_hosts"], json!(["web1"]));
        assert_eq!(shared["ansible_play_hosts_all"], json!(["web1"]));
        assert_eq!(shared["ansible_play_batch"], json!(["web1"]));
        // Asking the host's variables by name, which is what an argument spec does, answers for
        // the five as well: they are somewhere else, not gone.
        let host = HostVars {
            map: v.clone(),
            hostvars: Arc::default(),
            shared,
            ..HostVars::default()
        };
        assert_eq!(
            host.get("groups").map(|g| &g["web"]),
            Some(&json!(["web1"]))
        );
        assert_eq!(host.get("inventory_hostname"), Some(&json!("web1")));
        assert_eq!(
            v["playbook_dir"],
            json!(dir.join("play").display().to_string())
        );
        assert_eq!(
            v["inventory_dir"],
            json!(dir.join("inventory").display().to_string())
        );
        assert_eq!(
            v["inventory_file"],
            json!(dir.join("inventory/hosts.ini").display().to_string())
        );
        assert_eq!(v["omit"], json!(omit_token()));
        assert_eq!(v["ansible_check_mode"], json!(false));
    }
    {
        let inv = Inventory::parse_ini("[web]\nh1\nh2\nh3\n").unwrap();
        let mut store = VarStore::new(&inv, None, Path::new("."), Map::new()).unwrap();
        let v = store.shared_values(&Scope {
            play_hosts: vec!["h2".into(), "h3".into()],
            batch_hosts: vec!["h2".into()],
            all_play_hosts: vec!["h1".into(), "h2".into(), "h3".into()],
            ..Scope::default()
        });
        assert_eq!(v["ansible_play_hosts"], json!(["h2", "h3"]));
        assert_eq!(v["ansible_play_batch"], json!(["h2"]));
        assert_eq!(v["play_hosts"], json!(["h2"]));
        assert_eq!(v["ansible_play_hosts_all"], json!(["h1", "h2", "h3"]));
        // The map is cached, and the next batch is a different answer: asking again with other
        // lists has to rebuild it.
        let next = store.shared_values(&Scope {
            play_hosts: vec!["h3".into()],
            batch_hosts: vec!["h3".into()],
            all_play_hosts: vec!["h1".into(), "h2".into(), "h3".into()],
            ..Scope::default()
        });
        assert_eq!(next["ansible_play_hosts"], json!(["h3"]));
        assert_eq!(next["ansible_play_batch"], json!(["h3"]));
    }
}

/// The run's own tag lists decide the kubeconfig block; a host fact of either name loses.
#[test]
fn the_run_s_tag_lists_decide_the_kubeconfig_block() {
    let inventory = Inventory::parse_ini("h1\n").unwrap();
    let mut store = VarStore::new(&inventory, None, Path::new("."), Map::new()).unwrap();
    let templar = Templar::new(PathBuf::from("."));
    let condition = "kubectl_installed.rc == 0 and (k3s_server_copy_yaml.changed or 'kubeconfig' in ansible_run_tags)";
    let holds = |store: &mut VarStore| {
        let mut v = store.for_host("h1", &scope(&["h1"]));
        v.insert("kubectl_installed".into(), json!({"rc": 0}));
        v.insert("k3s_server_copy_yaml".into(), json!({"changed": false}));
        (
            templar.condition(condition, &v).unwrap(),
            v["ansible_run_tags"].clone(),
            v["ansible_skip_tags"].clone(),
        )
    };
    assert_eq!(holds(&mut store), (false, json!(["all"]), json!([])));

    store.set_untrusted_fact("h1", "ansible_run_tags", json!(["kubeconfig"]));
    assert_eq!(holds(&mut store), (false, json!(["all"]), json!([])));

    store.set_tags(&["kubeconfig".to_string()], &[]);
    assert_eq!(holds(&mut store), (true, json!(["kubeconfig"]), json!([])));
    store.set_tags(&["all".to_string()], &["foo".to_string()]);
    assert_eq!(holds(&mut store), (false, json!(["all"]), json!(["foo"])));
}

/// The view `hostvars` reads: one shared map for every host of the inventory, and a completed
/// one for a host the inventory does not carry.
#[test]
fn the_shared_hostvars_view_answers_for_every_host() {
    let tmp = tree();
    let dir = tmp.path();
    let (_, mut store) = store(dir);
    let view = store.hostvars_shared("web1");
    assert_eq!(view["web1"]["tier"], json!("ini"));
    assert!(
        view["web1"].get("hostvars").is_none(),
        "hostvars does not nest"
    );
    assert_eq!(
        view["web1"]["inventory_hostname"],
        json!("web1"),
        "the reference exposes a host's own identity through hostvars"
    );
    assert!(
        Arc::ptr_eq(&view, &store.hostvars_shared("web1")),
        "the inventory's hosts share one map"
    );
    let implicit = store.hostvars_shared("localhost");
    assert!(
        !view.contains_key("localhost"),
        "an implicit localhost is in no group"
    );
    assert_eq!(
        implicit["localhost"]["inventory_hostname"],
        json!("localhost")
    );
    assert_eq!(
        implicit["web1"]["tier"],
        json!("ini"),
        "and it still sees the inventory"
    );
}

/// What a template reads for each name: the host's own map, the facts over it, the
/// inventory-wide values over both.
fn effective(layers: &[&Map<String, Value>]) -> Map<String, Value> {
    let mut out = Map::new();
    for layer in layers {
        extend(&mut out, layer);
    }
    out
}

/// A small xorshift, so the cases are the same on every run and a failure names its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// Names shared between layers on purpose: magic ones, restricted ones, the namespace.
    fn name(&mut self) -> String {
        const NAMES: [&str; 14] = [
            "a",
            "b",
            "c",
            "ansible_x",
            "ansible_facts",
            "packages",
            "inventory_hostname",
            "omit",
            "ansible_forks",
            "groups",
            "ansible_play_hosts",
            "play_hosts",
            "ansible_connection",
            "ansible_host",
        ];
        NAMES[self.below(NAMES.len())].to_string()
    }
    fn value(&mut self, depth: u32) -> Value {
        match self.below(if depth == 0 { 3 } else { 5 }) {
            0 => json!(self.below(100)),
            1 => json!(format!("v{}", self.below(100))),
            2 => json!(null),
            3 => Value::Object(self.map(depth - 1)),
            _ => json!([self.value(depth - 1), self.value(depth - 1)]),
        }
    }
    fn map(&mut self, depth: u32) -> Map<String, Value> {
        (0..self.below(4))
            .map(|_| (self.name(), self.value(depth)))
            .collect()
    }
    fn hosts(&mut self) -> Vec<String> {
        ["h1", "h2"]
            .into_iter()
            .filter(|_| self.below(2) == 0)
            .map(String::from)
            .collect()
    }
    fn scope(&mut self) -> Scope {
        Scope {
            play_vars: self.map(2),
            vars_files: (0..self.below(3)).map(|_| self.map(2)).collect(),
            task_vars: self.map(2),
            role_defaults: self.map(2),
            role_vars: self.map(2),
            role_params: self.map(2),
            play_hosts: self.hosts(),
            batch_hosts: self.hosts(),
            all_play_hosts: vec!["h1".into(), "h2".into()],
        }
    }
    /// A write to the store: a gather, a trusted fact or an untrusted one.
    fn write(&mut self, store: &mut VarStore) {
        let host = ["h1", "h2", "localhost"][self.below(3)];
        match self.below(3) {
            0 => {
                store.gather_facts(host, &self.map(2));
            }
            1 => store.set_fact(host, &self.name(), self.value(2)),
            _ => store.set_untrusted_fact(host, &self.name(), self.value(2)),
        }
    }
}

/// The layers `layered_for_host` hands out read, name for name, like the one merged map.
#[test]
fn the_layered_view_reads_like_the_merged_one() {
    let seed = 0x5eed_1a7e_u64;
    let mut rng = Rng(seed);
    for case in 0..500 {
        let inventory = Inventory::parse_ini(
            [
                "[g]\nh1 a=1 ansible_x=inv\nh2 b=2\n",
                "h1\nh2 c=3 packages=inv\n",
            ][case % 2],
        )
        .unwrap();
        let extra = rng.map(1);
        let mut store = VarStore::new(&inventory, None, Path::new("."), extra).unwrap();
        let scope = rng.scope();
        for round in 0..2 {
            for _ in 0..rng.below(4) {
                rng.write(&mut store);
            }
            for host in ["h1", "h2", "localhost"] {
                let want =
                    effective(&[&store.for_host(host, &scope), &store.shared_values(&scope)]);
                let got = store.layered_for_host(host, &scope);
                assert_eq!(
                    effective(&[&got.map, &got.facts, &got.shared]),
                    want,
                    "seed {seed:#x}, case {case}, round {round}, host {host}"
                );
            }
        }
    }
}

/// A host leaving the play or writing a fact rebuilds only its own entries; a host's facts are
/// shared and carry nothing of the inventory.
#[test]
#[expect(clippy::too_many_lines)]
fn a_write_or_a_leaving_host_rebuilds_only_its_own_entries() {
    {
        let inventory = Inventory::parse_ini("h1\nh2\nh3\n").unwrap();
        let mut store = VarStore::new(&inventory, None, Path::new("."), Map::new()).unwrap();
        for host in ["h1", "h2", "h3"] {
            store.gather_facts(
                host,
                json!({"ansible_x": 1, "ansible_y": 2}).as_object().unwrap(),
            );
            store.set_fact(host, "ansible_x", json!("author"));
        }
        let all = scope(&["h1", "h2", "h3"]);
        for host in ["h1", "h2", "h3"] {
            assert_eq!(
                store.layered_for_host(host, &all).facts.get("ansible_x"),
                None
            );
        }
        let built = |store: &VarStore| {
            ["h1", "h2", "h3"].map(|h| store.facts_built.get(h).copied().unwrap_or(0))
        };
        assert_eq!(built(&store), [1, 1, 1]);
        // `h2` went unreachable: the lists move, and the two left read on.
        let left = scope(&["h1", "h3"]);
        for host in ["h1", "h3"] {
            let layers = store.layered_for_host(host, &left);
            assert_eq!(layers.facts.get("ansible_y"), Some(&json!(2)));
        }
        assert_eq!(built(&store), [1, 1, 1], "no entry of h1 or h3 built again");
        assert!(
            !store.host_facts.contains_key("h2"),
            "h2's entry left with it"
        );
        assert!(store.host_facts.contains_key("h1") && store.host_facts.contains_key("h3"));
    }
    {
        let hosts: Vec<String> = (0..50).map(|i| format!("h{i:02}")).collect();
        let text = hosts.join("\n");
        let inventory = Inventory::parse_ini(&format!("[web]\n{text}\n[db]\n{text}\n")).unwrap();
        let mut store = VarStore::new(&inventory, None, Path::new("."), Map::new()).unwrap();
        let names: Vec<&str> = hosts.iter().map(String::as_str).collect();
        let play = scope(&names);
        let facts = json!({"ansible_x": 1, "ansible_y": 2});
        for host in &names {
            store.gather_facts(host, facts.as_object().unwrap());
        }
        store.set_fact("h01", "ansible_x", json!("author"));
        let plain = store.layered_for_host("h00", &play);
        assert!(
            Arc::ptr_eq(&plain.facts, &store.gathered["h00"]),
            "shared, not copied"
        );
        let outranked = store.layered_for_host("h01", &play);
        for layers in [&plain, &outranked] {
            assert!(
                layers
                    .facts
                    .keys()
                    .all(|k| store.gathered["h01"].contains_key(k)),
                "facts only: {:?}",
                layers.facts.keys().collect::<Vec<_>>()
            );
        }
        assert_eq!(outranked.facts.len(), store.gathered["h01"].len() - 1);
        assert_eq!(
            store.host_facts.len(),
            1,
            "one entry, for the host with an outranked fact"
        );
    }
    {
        let inventory = Inventory::parse_ini("h1\nh2\n").unwrap();
        let mut store = VarStore::new(&inventory, None, Path::new("."), Map::new()).unwrap();
        let built = |store: &VarStore, host: &str| store.views_built.get(host).copied();
        let first = store.hostvars_shared("h1");
        assert_eq!(
            (built(&store, "h1"), built(&store, "h2")),
            (Some(1), Some(1))
        );

        store.set_untrusted_fact("h1", "r", json!({"rc": 0}));
        let second = store.hostvars_shared("h2");
        assert_eq!(second["h1"]["r"], json!({"rc": 0}), "the write is seen");
        assert_eq!(
            (built(&store, "h1"), built(&store, "h2")),
            (Some(2), Some(1))
        );
        assert!(
            first["h1"].get("r").is_none(),
            "a held map keeps what it saw"
        );

        store.gather_facts("h2", json!({"ansible_x": 1}).as_object().unwrap());
        store.set_fact("h2", "y", json!(2));
        let third = store.hostvars_shared("h1");
        assert_eq!(third["h2"]["ansible_x"], json!(1));
        assert_eq!(third["h2"]["y"], json!(2));
        assert_eq!(
            (built(&store, "h1"), built(&store, "h2")),
            (Some(2), Some(2))
        );
        assert!(
            second["h2"].get("y").is_none(),
            "a held map keeps what it saw"
        );

        let fourth = store.hostvars_shared("h2");
        assert!(
            Arc::ptr_eq(&third, &fourth),
            "nothing written, nothing rebuilt"
        );
    }
}

/// Every layer keeps its trust through the layered view: gathered and untrusted names stay text.
#[test]
#[expect(clippy::too_many_lines)]
fn every_layer_keeps_its_trust_through_the_layered_view() {
    let inventory = Inventory::parse_ini("h1 i=\"{{ 1 + 1 }}\"\n").unwrap();
    let payload = json!("{{ 1 + 1 }}");
    let one = |name: &str| -> Map<String, Value> {
        [(name.to_string(), payload.clone())].into_iter().collect()
    };
    let extra = one("e");
    let mut store = VarStore::new(&inventory, None, Path::new("."), extra).unwrap();
    store.gather_facts(
        "h1",
        json!({"g": "{{ 1 + 1 }}", "gp": "{{ 1 + 1 }}", "groups": "{{ 1 + 1 }}"})
            .as_object()
            .unwrap(),
    );
    store.set_fact("h1", "sf", payload.clone());
    store.set_untrusted_fact("h1", "su", payload.clone());
    let mut play_vars = one("p");
    play_vars.insert("gp".into(), json!("author"));
    play_vars.insert("built".into(), json!("{{ g }}"));
    let scope = Scope {
        play_vars,
        vars_files: vec![one("f")],
        task_vars: one("t"),
        role_defaults: one("d"),
        role_vars: one("rv"),
        role_params: one("rp"),
        ..scope(&["h1"])
    };
    let templar = Templar::new(PathBuf::from("."));
    let untrusted = store.untrusted_of("h1", &scope);
    let untrusted_hosts = store.untrusted_hosts();
    let hostvars = store.hostvars_shared("h1");
    let read = |map: &Map<String, Value>,
                shared: &Arc<Map<String, Value>>,
                facts: &Arc<Map<String, Value>>| {
        let (resolved, soiled) = templar.resolve_vars_tainted(Vars {
            map,
            hostvars: Some(&hostvars),
            shared: Some(shared),
            facts: Some(facts),
            untrusted: Some(&untrusted),
            untrusted_hosts: Some(&untrusted_hosts),
        });
        let names = [
            "d", "i", "g", "gp", "p", "f", "rv", "t", "sf", "su", "rp", "e", "built", "groups",
        ];
        let answers: Vec<_> = names
            .iter()
            .map(|name| {
                let text = format!("{{{{ {name} }}}}");
                templar
                    .render(
                        &text,
                        Vars {
                            map: &resolved,
                            hostvars: Some(&hostvars),
                            shared: Some(shared),
                            facts: Some(facts),
                            untrusted: Some(&soiled),
                            untrusted_hosts: Some(&untrusted_hosts),
                        },
                    )
                    .unwrap()
            })
            .collect();
        (answers, soiled)
    };
    let merged = store.for_host("h1", &scope);
    let inventory_wide = store.shared_values(&scope);
    let layers = store.layered_for_host("h1", &scope);
    let (answers, soiled) = read(&layers.map, &layers.shared, &layers.facts);
    assert_eq!(
        (answers.clone(), soiled.clone()),
        read(&merged, &inventory_wide, &Arc::default())
    );
    let two = json!(2);
    assert_eq!(
        answers,
        [
            &two,
            &two,
            &payload,
            &json!("author"),
            &two,
            &two,
            &two,
            &two,
            &two,
            &payload,
            &two,
            &two,
            &payload,
            &inventory_wide["groups"]
        ]
        .map(Clone::clone)
    );
    assert_eq!(answers[13], json!({"all": ["h1"], "ungrouped": ["h1"]}));
    for name in ["g", "su", "built"] {
        assert!(soiled.contains(name), "{name} is data");
    }
    assert!(
        !soiled.contains("gp"),
        "the author's value over a fact is not"
    );
}

/// A gathered fact reaches a template lookup, with the same text as through the merged map.
#[test]
fn a_gathered_fact_reaches_a_template_lookup() {
    let tmp = scratch();
    let dir = tmp.path();
    write(dir, "t.j2", "{{ ansible_x }} {{ template_path }}");
    let inventory = Inventory::parse_ini("h1\n").unwrap();
    let mut store = VarStore::new(&inventory, None, dir, Map::new()).unwrap();
    store.gather_facts(
        "h1",
        json!({"ansible_x": "hello", "template_path": "/from/a/host"})
            .as_object()
            .unwrap(),
    );
    let play = scope(&["h1"]);
    let templar = Templar::new(dir.to_path_buf());
    let untrusted = store.untrusted_of("h1", &play);
    let layers = store.layered_for_host("h1", &play);
    let merged = store.for_host("h1", &play);
    let inventory_wide = store.shared_values(&play);
    let render = |map: &Map<String, Value>, facts: &Arc<Map<String, Value>>| {
        templar
            .render(
                "{{ lookup('template', 't.j2') }}",
                Vars {
                    map,
                    hostvars: None,
                    shared: Some(&inventory_wide),
                    facts: Some(facts),
                    untrusted: Some(&untrusted),
                    untrusted_hosts: None,
                },
            )
            .unwrap()
    };
    let got = render(&layers.map, &layers.facts);
    assert_eq!(got, render(&merged, &Arc::default()));
    assert_eq!(got, json!("hello t.j2"));
}

/// A short hostname drops the domain; a host sharing a group's name still gets its variables.
#[test]
fn a_short_hostname_drops_the_domain_and_a_homonym_keeps_its_variables() {
    {
        let inv = Inventory::parse_ini("web1.example.com\n").unwrap();
        let mut store = VarStore::new(&inv, None, Path::new("."), Map::new()).unwrap();
        let v = store.for_host("web1.example.com", &scope(&["web1.example.com"]));
        assert_eq!(v["inventory_hostname_short"], json!("web1"));
    }
    {
        // Ansible warns "Found both group and host with same name" and runs the host.
        let inv = Inventory::parse_ini("[web]\ndb x=1\n[db]\n[web:vars]\nfrom=group\n").unwrap();
        let mut store = VarStore::new(&inv, None, Path::new("."), Map::new()).unwrap();
        let v = store.for_host("db", &scope(&["db"]));
        assert_eq!(v["inventory_hostname"], json!("db"));
        assert_eq!(v["x"], json!(1));
        assert_eq!(v["from"], json!("group"));
    }
}

/// Extra vars take key=value, JSON and files; a broken vars file names itself, a comment-only
/// one is empty and a scalar is refused.
#[test]
fn extra_vars_and_vars_files_are_read_or_refused_by_name() {
    {
        let tmp = tree();
        let dir = tmp.path();
        write(dir, "extra.yml", "from_file: 1\nlayer: file\n");
        let parsed = parse_extra_vars(
            &[
                "a=1 b=two".to_string(),
                r#"{"c": [1]}"#.to_string(),
                "@extra.yml".to_string(),
            ],
            dir,
        )
        .unwrap();
        assert_eq!(parsed["a"], json!("1"), "key=value extra vars are strings");
        assert_eq!(parsed["b"], json!("two"));
        assert_eq!(parsed["c"], json!([1]));
        assert_eq!(parsed["from_file"], json!(1));
        assert_eq!(parsed["layer"], json!("file"));
    }
    {
        let tmp = tree();
        let dir = tmp.path();
        write(dir, "play/group_vars/all.yml", "a: [1,\n");
        let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
        let err = VarStore::new(
            &inv,
            Some(&dir.join("inventory/hosts.ini")),
            &dir.join("play"),
            Map::new(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("all.yml"));
    }
    {
        let tmp = scratch();
        let dir = tmp.path();
        for text in [
            "---\n# Empty file\n",
            "---\n# This space intentionally left blank.\n",
        ] {
            let file = dir.join("main.yml");
            std::fs::write(&file, text).unwrap();
            assert_eq!(load_vars_file(&file).unwrap(), Map::new(), "{text:?}");
        }
        for text in ["42\n", "\"x\"\n", "---\n''\n"] {
            let file = dir.join("main.yml");
            std::fs::write(&file, text).unwrap();
            let err = load_vars_file(&file).unwrap_err();
            assert!(
                format!("{err:#}").contains("a variables file must contain a mapping"),
                "{text:?}: {err:#}"
            );
        }
    }
}

/// `clear_gathered` empties what a module gathered and leaves a `set_fact`; a refreshed
/// inventory shows its new host in `groups` at once.
#[test]
fn clearing_facts_keeps_set_facts_and_a_refresh_shows_the_new_host() {
    let tmp = scratch();
    let inv = Inventory::parse_ini(
        "[g]
h1
",
    )
    .unwrap();
    let mut store = VarStore::new(&inv, None, tmp.path(), Map::new()).unwrap();
    let gathered = json!({"ansible_os_family": "Debian"});
    store.gather_facts("h1", gathered.as_object().unwrap());
    store.set_fact("h1", "mine", json!(1));
    let at = scope(&["h1"]);
    let before = store.for_host("h1", &at);
    assert_eq!(before["ansible_os_family"], "Debian");
    store.clear_gathered(&["h1".to_string()]);
    let after = store.for_host("h1", &at);
    assert!(!after.contains_key("ansible_os_family"), "{after:?}");
    assert_eq!(after["ansible_facts"], json!({}), "{after:?}");
    assert_eq!(after["mine"], 1);

    assert_eq!(store.shared_values(&at)["groups"]["g"], json!(["h1"]));
    store.refresh_inventory(
        &Inventory::parse_ini(
            "[g]
h1
h2
",
        )
        .unwrap(),
    );
    assert_eq!(store.shared_values(&at)["groups"]["g"], json!(["h1", "h2"]));
    assert!(store.hostvars_shared("h1").contains_key("h2"));
}

/// A refresh reads the sources anew but keeps the hosts and groups `add_host` made.
#[test]
fn a_refresh_keeps_the_added_hosts() {
    let tmp = scratch();
    let inv = Inventory::parse_ini(
        "[g]
h1
",
    )
    .unwrap();
    let mut store = VarStore::new(&inv, None, tmp.path(), Map::new()).unwrap();
    store.add_host("dyn", &["g_dyn".to_string()], &Map::new());
    let fresh = Inventory::parse_ini(
        "[g]
h2
",
    )
    .unwrap();
    let kept = store.refresh_inventory(&fresh);
    let groups = kept.groups();
    assert_eq!(groups["g"], ["h2"]);
    assert_eq!(groups["g_dyn"], ["dyn"]);
    assert!(store.take_inventory_changes().is_none());
}

/// A host and a group added mid-run show in `groups` and `hostvars` at once, an existing
/// host keeps its groups and gains the new ones, and the grown inventory is taken once.
#[test]
fn added_hosts_and_groups_are_visible_at_once_and_handed_over_once() {
    let tmp = scratch();
    let inv = Inventory::parse_ini(
        "[g]
h1
",
    )
    .unwrap();
    let mut store = VarStore::new(&inv, None, tmp.path(), Map::new()).unwrap();
    let at = scope(&["h1"]);
    assert!(store.hostvars_shared("h1").contains_key("h1"));
    assert_eq!(store.shared_values(&at)["groups"]["g"], json!(["h1"]));

    let vars = json!({"foo": "bar"});
    store.add_host("n1", &["g1".into(), "g2".into()], vars.as_object().unwrap());
    store.add_host(
        "n1",
        &["g3".into()],
        json!({"foo": "baz"}).as_object().unwrap(),
    );
    let groups = store.shared_values(&at)["groups"].clone();
    assert_eq!(groups["g1"], json!(["n1"]));
    assert_eq!(groups["all"], json!(["h1", "n1"]));
    let view = store.hostvars_shared("h1");
    assert_eq!(view["n1"]["foo"], "baz");
    assert_eq!(groups["g3"], json!(["n1"]));

    store.add_to_group("h1", "web", &["roles".into()]);
    let groups = store.shared_values(&at)["groups"].clone();
    assert_eq!(groups["web"], json!(["h1"]));
    assert_eq!(
        groups["roles"],
        json!(["h1"]),
        "a parent holds its children's hosts"
    );

    let grown = store.take_inventory_changes().expect("the inventory grew");
    assert_eq!(grown.groups()["all"], ["h1", "n1"]);
    assert!(store.take_inventory_changes().is_none());
}

/// A value built from a fact that holds a template stays text: it is never rendered again.
#[test]
fn an_add_host_value_that_holds_a_template_is_data() {
    let tmp = scratch();
    let inv = Inventory::parse_ini(
        "[g]
h1
",
    )
    .unwrap();
    let mut store = VarStore::new(&inv, None, tmp.path(), Map::new()).unwrap();
    let vars =
        json!({"note": "{{ lookup('pipe','id') }}", "plain": "x", "ansible_host": "10.0.0.9"});
    store.add_host("n1", &[], vars.as_object().unwrap());
    let seen = store.for_host("n1", &scope(&["n1"]));
    assert_eq!(seen["note"], "{{ lookup('pipe','id') }}");
    assert_eq!(seen["plain"], "x");
    assert_eq!(seen["ansible_host"], "10.0.0.9");
    assert!(store.untrusted_of("n1", &scope(&["n1"])).contains("note"));
    assert!(!store.untrusted_of("n1", &scope(&["n1"])).contains("plain"));
}

/// A vaulted vars file is read with the run's secrets; one it cannot read is named.
#[test]
fn a_vaulted_vars_file_is_decrypted_or_refused_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("v.yml");
    let sealed = crate::vault::encrypt(
        b"a: '{{ b }}'
n: 5
",
        b"pw",
        Some("dev"),
    )
    .unwrap();
    std::fs::write(&file, sealed).unwrap();
    crate::vault::install(crate::vault::Secrets::new(vec![b"pw".to_vec()]));
    assert_eq!(
        load_vars_file(&file).unwrap(),
        json!({"a": "{{ b }}", "n": 5})
            .as_object()
            .cloned()
            .unwrap()
    );
    crate::vault::install(crate::vault::Secrets::new(vec![b"nope".to_vec()]));
    let err = load_vars_file(&file).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Invalid vars_files file '{}': Decryption failed (no vault secrets were found that could decrypt).",
            file.display()
        )
    );
    assert!(err.is::<VaultFile>());
}

/// The implicit `localhost` gets the controller's interpreter, at the layer of its implicit
/// connection; a named host or nothing resolved leaves it unset.
#[rstest::rstest]
#[case::implicit(Some("/ctl/python3"), "", &[], false, Some("/ctl/python3"))]
#[case::nothing_resolves(None, "", &[], false, None)]
#[case::named_in_the_inventory(Some("/ctl/python3"), "localhost\n", &[], false, None)]
#[case::host_vars_file(Some("/ctl/python3"), "", &[("host_vars/localhost.yml", "ansible_python_interpreter: /hv/python\n")], false, Some("/hv/python"))]
#[case::play_vars(Some("/ctl/python3"), "", &[], true, Some("/play/python"))]
#[case::group_vars_all_loses(Some("/ctl/python3"), "", &[("group_vars/all.yml", "ansible_python_interpreter: /gv/python
")], false, Some("/ctl/python3"))]
fn the_implicit_localhost_runs_under_the_controller_python(
    #[case] controller: Option<&str>,
    #[case] hosts: &str,
    #[case] files: &[(&str, &str)],
    #[case] play_var: bool,
    #[case] expected: Option<&str>,
) {
    let tmp = scratch();
    let dir = tmp.path();
    write(dir, "inventory/hosts.ini", &format!("[web]\nweb1\n{hosts}"));
    for (rel, text) in files {
        write(dir, &format!("inventory/{rel}"), text);
    }
    let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
    let mut store = VarStore::new(
        &inv,
        Some(&dir.join("inventory/hosts.ini")),
        &dir.join("play"),
        Map::new(),
    )
    .unwrap();
    crate::runtime::resolve::set_test_python(controller);
    let mut sc = scope(&["localhost"]);
    if play_var {
        sc.play_vars = json!({"ansible_python_interpreter": "/play/python"})
            .as_object()
            .unwrap()
            .clone();
    }
    let got = store
        .for_host("localhost", &sc)
        .get("ansible_python_interpreter")
        .and_then(Value::as_str)
        .map(str::to_owned);
    assert_eq!(got.as_deref(), expected);
}

/// `-e` beats the controller's interpreter too.
#[test]
fn an_extra_var_beats_the_controller_python_of_the_implicit_localhost() {
    let tmp = tree();
    let dir = tmp.path();
    let inv = Inventory::load(&dir.join("inventory/hosts.ini")).unwrap();
    let extra = json!({"ansible_python_interpreter": "/e/python"})
        .as_object()
        .unwrap()
        .clone();
    let mut store = VarStore::new(
        &inv,
        Some(&dir.join("inventory/hosts.ini")),
        &dir.join("play"),
        extra,
    )
    .unwrap();
    crate::runtime::resolve::set_test_python(Some("/ctl/python3"));
    let v = store.for_host("localhost", &scope(&["localhost"]));
    assert_eq!(v["ansible_python_interpreter"], json!("/e/python"));
}

/// A role default sits below the inventory layer, so the implicit value beats it.
#[test]
fn a_role_default_loses_to_the_controller_python_of_the_implicit_localhost() {
    let tmp = tree();
    let dir = tmp.path();
    let (_, mut store) = store(dir);
    crate::runtime::resolve::set_test_python(Some("/ctl/python3"));
    let mut sc = scope(&["localhost"]);
    sc.role_defaults = json!({"ansible_python_interpreter": "/role/python"})
        .as_object()
        .unwrap()
        .clone();
    let v = store.for_host("localhost", &sc);
    assert_eq!(v["ansible_python_interpreter"], json!("/ctl/python3"));
}
