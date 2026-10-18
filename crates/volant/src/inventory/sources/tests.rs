// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use rstest::rstest;

fn quiet_cx(config: &PluginConfig) -> ParseCx<'_> {
    ParseCx::new(config, &|| unreachable!("no helper is started"))
}

/// Only a collection plugin starts the helper; the other YAML files are read without it.
#[rstest]
#[case::builtin(
    "plugin: ansible.builtin.constructed
",
    0
)]
#[case::no_key(
    "hosts: {}
",
    0
)]
#[case::unknown_short_name(
    "plugin: nothing
",
    0
)]
#[case::collection(
    "plugin: community.general.nmap
",
    1
)]
fn the_helper_starts_only_for_a_collection_plugin(#[case] body: &str, #[case] starts: usize) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("inv.yml"), body).unwrap();
    let count = std::sync::atomic::AtomicUsize::new(0);
    let start = || {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Bridge::with_start(Box::new(|| Err(anyhow::anyhow!("no python"))))
    };
    let source = Source::new(&dir.path().join("inv.yml").to_string_lossy());
    load_sources_with(&[source], &PluginConfig::default(), &start);
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), starts);
}

/// What the helper answers becomes hosts, groups and data; a failure keeps what was said.
#[test]
fn a_collection_plugin_s_answer_is_read_as_data() {
    let config = PluginConfig::default();
    let cx = quiet_cx(&config);
    let source = Source::new("/tmp/inv.yml");
    let mut inventory = Inventory::default();
    let answer = serde_json::json!({
        "verbose": ["Using inventory plugin 'x.y.z' to process inventory source '/tmp/inv.yml'"],
        "warnings": ["Found variable using reserved name 'name'."],
        "hosts": [{"name": "h1", "vars": {
            "a": {"__ansible_unsafe": "{{ boom }}"}, "n": 3, "ok": "plain"}}],
        "groups": [
            {"name": "all", "hosts": ["h1"], "children": ["g"], "vars": {}},
            {"name": "g", "hosts": ["h1"], "children": [], "vars": {}},
        ],
    });
    super::super::collection::read(&answer, &source, &mut inventory, &cx).unwrap();
    assert_eq!(
        cx.notes.into_inner(),
        [
            Note::Verbose(
                1,
                "Using inventory plugin 'x.y.z' to process inventory source '/tmp/inv.yml'".into()
            ),
            Note::Warning("Found variable using reserved name 'name'.".into()),
        ]
    );
    assert_eq!(
        inventory.dump()["_meta"]["hostvars"]["h1"],
        serde_json::json!({"a": {"__ansible_unsafe": "{{ boom }}"}, "n": 3, "ok": "plain"})
    );
    let cx = quiet_cx(&config);
    let failed = serde_json::json!({"verbose": [], "warnings": ["w"], "error": "boom"});
    let err = super::super::collection::read(&failed, &source, &mut inventory, &cx).unwrap_err();
    assert_eq!(err.message, "boom");
    assert!(cx.notes.into_inner().contains(&Note::Warning("w".into())));
}

const DECLINED: &str = "declined parsing <dir>/{} as it did not pass its verify_file() method";

/// The `-vvv` lines of the chain for one source, as measured on the reference.
#[rstest]
#[case::ini("hosts.ini", &["host_list", "script", "auto", "yaml"], "ini")]
#[case::no_extension("hosts", &["host_list", "script", "auto"], "ini")]
#[case::toml_is_not_ini("hosts.toml", &["host_list", "script", "auto", "yaml", "ini"], "")]
fn a_file_is_offered_to_each_plugin_in_turn(
    #[case] file: &str,
    #[case] declined: &[&str],
    #[case] parsed_by: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(file),
        "[web]
h1
",
    )
    .unwrap();
    let shown = dir.path().display().to_string();
    let source = Source::new(&dir.path().join(file).to_string_lossy());
    let loaded = load_sources(&[source], &PluginConfig::default());
    let lines: Vec<String> = loaded
        .notes
        .iter()
        .filter_map(|n| match n {
            Note::Verbose(3, text) => Some(text.replace(&shown, "<dir>")),
            _ => None,
        })
        .collect();
    let mut want: Vec<String> = declined
        .iter()
        .map(|p| format!("{p} {}", DECLINED.replace("{}", file)))
        .collect();
    if parsed_by.is_empty() {
        assert!(
            loaded.error.is_none() && !loaded.parsed,
            "only toml takes the file, and it cannot read INI"
        );
    } else {
        want.push(format!(
            "Parsed <dir>/{file} inventory source with {parsed_by} plugin"
        ));
        assert!(loaded.parsed);
    }
    assert_eq!(lines, want);
}

#[test]
fn a_host_list_is_shown_as_given_and_ignored_names_are_skipped() {
    let loaded = load_sources(&[Source::new("a,b,")], &PluginConfig::default());
    assert_eq!(
        loaded.notes,
        [Note::Verbose(
            3,
            "Parsed a,b, inventory source with host_list plugin".into()
        )]
    );
    let config = PluginConfig {
        ignore_patterns: vec!["^sub".into()],
        ..PluginConfig::default()
    };
    let ignored = ignored_names(&config).unwrap();
    for name in [
        ".hidden.ini",
        "group_vars",
        "x.retry",
        "notes~",
        "sub",
        "a.txt",
    ] {
        assert!(ignored.is_match(name), "{name} is skipped");
    }
    for name in ["hosts.ini", "hosts", "my_group_vars"] {
        assert!(!ignored.is_match(name), "{name} is read");
    }
}
