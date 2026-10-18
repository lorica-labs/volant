// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// Sorted and free of duplicates, so a missing keyword is easy to spot and the lookups above
/// cannot be shadowed by a second row.
#[test]
fn the_tables_are_sorted_and_free_of_duplicates() {
    for table in [
        TASK_KEYWORDS,
        PLAY_KEYWORDS,
        LOOP_CONTROL_KEYWORDS,
        BLOCK_KEYWORDS,
        HANDLER_KEYWORDS,
    ] {
        let names: Vec<&str> = table.iter().map(|k| k.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted);
    }
    let mut sections = BLOCK_SECTIONS.to_vec();
    sections.sort_unstable();
    sections.dedup();
    assert_eq!(BLOCK_SECTIONS, sections.as_slice());
}

/// The keyword tables are ansible-core 2.19.12's own `fattributes` lists, less two internal
/// spellings and plus the keys it resolves first.
#[test]
fn the_tables_are_the_reference_s_own_attribute_lists() {
    let task_fattributes = [
        "action",
        "any_errors_fatal",
        "args",
        "async",
        "become",
        "become_exe",
        "become_flags",
        "become_method",
        "become_user",
        "changed_when",
        "check_mode",
        "collections",
        "connection",
        "debugger",
        "delay",
        "delegate_facts",
        "delegate_to",
        "diff",
        "environment",
        "failed_when",
        "ignore_errors",
        "ignore_unreachable",
        "loop",
        "loop_control",
        "module_defaults",
        "name",
        "no_log",
        "notify",
        "poll",
        "port",
        "register",
        "remote_user",
        "retries",
        "run_once",
        "tags",
        "throttle",
        "timeout",
        "until",
        "vars",
        "when",
    ];
    let lookups = [
        "config",
        "csvfile",
        "dict",
        "env",
        "file",
        "fileglob",
        "first_found",
        "indexed_items",
        "ini",
        "inventory_hostnames",
        "items",
        "lines",
        "list",
        "nested",
        "password",
        "pipe",
        "random_choice",
        "sequence",
        "subelements",
        "template",
        "together",
        "unvault",
        "url",
        "varnames",
        "vars",
    ];
    let mut expected: Vec<String> = task_fattributes.iter().map(ToString::to_string).collect();
    expected.push("local_action".to_string());
    expected.extend(lookups.iter().map(|l| format!("with_{l}")));
    expected.sort_unstable();
    let ours: Vec<String> = TASK_KEYWORDS.iter().map(|k| k.name.to_string()).collect();
    assert_eq!(ours, expected);

    let play_fattributes = [
        "any_errors_fatal",
        "become",
        "become_exe",
        "become_flags",
        "become_method",
        "become_user",
        "check_mode",
        "collections",
        "connection",
        "debugger",
        "diff",
        "environment",
        "fact_path",
        "force_handlers",
        "gather_facts",
        "gather_subset",
        "gather_timeout",
        "handlers",
        "hosts",
        "ignore_errors",
        "ignore_unreachable",
        "max_fail_percentage",
        "module_defaults",
        "name",
        "no_log",
        "order",
        "port",
        "post_tasks",
        "pre_tasks",
        "remote_user",
        "roles",
        "run_once",
        "serial",
        "strategy",
        "tags",
        "tasks",
        "throttle",
        "timeout",
        "vars",
        "vars_files",
        "vars_prompt",
    ];
    let ours: Vec<&str> = PLAY_KEYWORDS.iter().map(|k| k.name).collect();
    assert_eq!(ours, play_fattributes);

    // `LoopControl.fattributes`, read the same way. The reference refuses any other
    // sub-key outright, so this list is the whole grammar under `loop_control`.
    let loop_control_fattributes = [
        "break_when",
        "extended",
        "extended_allitems",
        "index_var",
        "label",
        "loop_var",
        "pause",
    ];
    let ours: Vec<&str> = LOOP_CONTROL_KEYWORDS.iter().map(|k| k.name).collect();
    assert_eq!(ours, loop_control_fattributes);

    // `Block.fattributes`, read the same way on the same release: 31 names, `name` among
    // them. Written out rather than derived from the task list, because it is not that
    // list minus the loop keywords: it also has `delegate_to`, `delegate_facts`, `notify`
    // and `collections`, and the derivation that guessed otherwise would have refused
    // `name:` on a block, which the reference runs.
    let block_fattributes = [
        "always",
        "any_errors_fatal",
        "become",
        "become_exe",
        "become_flags",
        "become_method",
        "become_user",
        "block",
        "check_mode",
        "collections",
        "connection",
        "debugger",
        "delegate_facts",
        "delegate_to",
        "diff",
        "environment",
        "ignore_errors",
        "ignore_unreachable",
        "module_defaults",
        "name",
        "no_log",
        "notify",
        "port",
        "remote_user",
        "rescue",
        "run_once",
        "tags",
        "throttle",
        "timeout",
        "vars",
        "when",
    ];
    let ours: Vec<&str> = BLOCK_KEYWORDS.iter().map(|k| k.name).collect();
    assert_eq!(ours, block_fattributes);

    // `Handler.fattributes` read the same way is `Task.fattributes` plus `listen` and
    // nothing else, which is why this table holds the difference alone.
    let ours: Vec<&str> = HANDLER_KEYWORDS.iter().map(|k| k.name).collect();
    assert_eq!(ours, ["listen"]);
    assert!(
        task_keyword("listen").is_none(),
        "a handler keyword is not a task keyword; a name in both would hide one of the two"
    );
}

/// A block section is never also a task keyword: the loader tells a block from a task by
/// looking for one of these keys, so a name in both would make one of the two unreachable.
#[test]
fn the_block_sections_are_block_keywords_and_not_task_keywords() {
    for section in BLOCK_SECTIONS {
        assert!(task_keyword(section).is_none(), "{section}");
        assert!(block_keyword(section).is_some(), "{section}");
    }
}

/// `run_once` is the only keyword any table declares a barrier.
#[test]
fn run_once_is_the_only_barrier_keyword() {
    let declared: Vec<&str> = [
        TASK_KEYWORDS,
        PLAY_KEYWORDS,
        LOOP_CONTROL_KEYWORDS,
        BLOCK_KEYWORDS,
        HANDLER_KEYWORDS,
    ]
    .iter()
    .flat_map(|table| table.iter())
    .filter(|k| k.barrier)
    .map(|k| k.name)
    .collect();
    assert!(!declared.is_empty());
    assert!(
        declared.iter().all(|name| *name == "run_once"),
        "{declared:?}"
    );
}

/// `is_barrier` has exactly one caller, `PlayTask::barrier`, and it only ever asks about the
/// literal `"run_once"` - a name that is both present in every table and marked `barrier`.
#[test]
fn is_barrier_reads_both_the_name_and_the_flag() {
    assert!(is_barrier("run_once"));
    // `name` is a real keyword in every table and is not a barrier: a lookup that only
    // checks the name would say yes.
    assert!(!is_barrier("name"));
    // Not a keyword at all: a lookup that ignores its argument would say yes.
    assert!(!is_barrier("not_a_real_keyword"));
}

/// The `not yet` legend says a keyword in a dynamically included file stops the statement, not
/// the run.
#[test]
fn the_not_yet_legend_names_the_include_path() {
    let page = documentation();
    let legend = page
        .lines()
        .find(|l| l.starts_with(r#"<li><span class="kw kw-notyet">"#))
        .expect("the legend has a not yet entry");
    assert!(
        legend.contains("before anything connects") && legend.contains("dynamic"),
        "{legend}"
    );
}

/// The keyword page is regenerated from the keyword tables and matches them.
#[test]
fn the_keyword_page_matches_the_tables() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/src/content/docs/reference/keywords.md");
    let expected = documentation();
    if std::env::var_os("VOLANT_UPDATE_DOCS").is_some() {
        std::fs::write(&path, &expected).unwrap();
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        actual, expected,
        "run `just docs-keywords` to regenerate docs/src/content/docs/reference/keywords.md"
    );
}
