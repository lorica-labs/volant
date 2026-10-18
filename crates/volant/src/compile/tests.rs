// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rstest::rstest;
use serde_json::{Map, Value, json};

use super::include::include_role_options;
use super::*;
use crate::inventory::Host;
use crate::playbook::{PlayTask, parse};
use crate::roles::RoleSearch;
use crate::template::Templar;
use crate::testing::{compiled, play, scratch};

/// A file beside the importer wins; the role's own `tasks/` is the fallback.
#[test]
fn beside_or_in_role_prefers_beside_and_falls_back_to_the_role() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let file_dir = dir.join("tasks/sub");
    let role_tasks = dir.join("tasks");
    std::fs::create_dir_all(&file_dir).unwrap();
    std::fs::create_dir_all(&role_tasks).unwrap();

    // Neither exists: the path reported is the one beside the importer.
    assert_eq!(
        beside_or_in_role(&file_dir, Some(&dir), "warning_facts.yml"),
        file_dir.join("warning_facts.yml")
    );

    // Only the role has it: the fallback takes over.
    std::fs::write(role_tasks.join("warning_facts.yml"), "").unwrap();
    assert_eq!(
        beside_or_in_role(&file_dir, Some(&dir), "warning_facts.yml"),
        role_tasks.join("warning_facts.yml")
    );

    // Both have it: the one beside the importer wins.
    std::fs::write(file_dir.join("warning_facts.yml"), "").unwrap();
    assert_eq!(
        beside_or_in_role(&file_dir, Some(&dir), "warning_facts.yml"),
        file_dir.join("warning_facts.yml")
    );
}

/// Compiles the first play with the tag selection given, and **without** gathering facts, so
/// the indices every test below reads are the ones the playbook wrote.
///
/// `gather_facts` defaults to on, and it lays a step at index 0, so leaving it on here would
/// shift the whole list by one and say nothing about the layout these tests are about.
/// `a_play_that_gathers_facts_opens_with_the_setup_module` is the one that turns it back on.
fn selected(text: &str, selection: &TagSelection) -> Compiled {
    let mut pb = play(text);
    pb.plays[0].gather_facts = false;
    compile(&pb.plays[0], &RoleSearch::default(), selection).unwrap_or_else(|e| panic!("{e:#}"))
}

fn selection(run: &[&str], skip: &[&str]) -> TagSelection {
    TagSelection::new(
        run.iter().map(ToString::to_string).collect(),
        skip.iter().map(ToString::to_string).collect(),
    )
}

/// The sizes of the batches `serial` cuts, which is what the tests below compare.
fn cut(serial: Option<Value>, hosts: usize) -> anyhow::Result<Vec<usize>> {
    let hosts: Vec<Host> = (1..=hosts)
        .map(|i| Host {
            name: format!("h{i}"),
            ..Host::default()
        })
        .collect();
    let templar = Templar::new(PathBuf::from("."));
    Ok(batches(&hosts, serial.as_ref(), &templar, &Map::new())?
        .iter()
        .map(Vec::len)
        .collect())
}

/// Every shape of `serial` measured on ansible-core 2.19.12 over three hosts, cut the same way
/// it cuts them, and the ones nobody can read, refused by keyword.
#[rstest]
#[case::two(json!(2), Some(vec![2, 1]))]
#[case::one(json!(1), Some(vec![1, 1, 1]))]
#[case::zero(json!(0), Some(vec![3]))]
#[case::negative(json!(-1), Some(vec![3]))]
#[case::more_than_hosts(json!(10), Some(vec![3]))]
#[case::boolean(json!(true), Some(vec![1, 1, 1]))]
#[case::text_number(json!("2"), Some(vec![2, 1]))]
#[case::ten_percent(json!("10%"), Some(vec![1, 1, 1]))]
#[case::zero_percent(json!("0%"), Some(vec![1, 1, 1]))]
#[case::thirty_four_percent(json!("34%"), Some(vec![1, 1, 1]))]
#[case::fifty_percent(json!("50%"), Some(vec![1, 1, 1]))]
#[case::hundred_percent(json!("100%"), Some(vec![3]))]
#[case::list(json!([1, 5]), Some(vec![1, 2]))]
#[case::list_with_percent(json!([1, "50%"]), Some(vec![1, 1, 1]))]
#[case::empty_list(json!([]), Some(vec![3]))]
#[case::word(json!("abc"), None)]
#[case::bad_percent(json!("x%"), None)]
#[case::mapping(json!({}), None)]
#[case::list_with_word(json!([1, "abc"]), None)]
#[case::percent_with_trailing_space(json!("50% "), None)]
fn serial_cuts_what_the_reference_cuts(#[case] serial: Value, #[case] sizes: Option<Vec<usize>>) {
    match (cut(Some(serial.clone()), 3), sizes) {
        (Ok(got), Some(want)) => assert_eq!(got, want, "serial: {serial}"),
        (Err(err), None) => {
            let err = format!("{err:#}");
            assert!(
                err.contains("Error processing keyword 'serial'"),
                "{serial}: {err}"
            );
        }
        (got, want) => panic!("serial {serial}: got {got:?}, wanted {want:?}"),
    }
    assert_eq!(cut(None, 3).unwrap(), vec![3], "no serial is one batch");
}

/// `serial` is rendered first against the play's variables; a name with no value stops the
/// run (measured: exit 4, `Error processing keyword 'serial'`).
#[test]
fn a_templated_serial_is_rendered_first() {
    let hosts: Vec<Host> = ["h1", "h2", "h3"]
        .iter()
        .map(|name| Host {
            name: (*name).to_string(),
            ..Host::default()
        })
        .collect();
    let templar = Templar::new(PathBuf::from("."));
    let vars: Map<String, Value> = json!({ "n": 2 }).as_object().cloned().unwrap();
    let sizes: Vec<usize> = batches(&hosts, Some(&json!("{{ n }}")), &templar, &vars)
        .unwrap()
        .iter()
        .map(Vec::len)
        .collect();
    assert_eq!(sizes, vec![2, 1]);
    let err = format!(
        "{:#}",
        batches(&hosts, Some(&json!("{{ n }}")), &templar, &Map::new()).unwrap_err()
    );
    assert!(err.contains("Error processing keyword 'serial'"), "{err}");
}

/// Whether a step is one of the flush points the compiler puts in itself. They carry no task
/// and stand at the end of a section, so the helpers below leave them out: what these tests
/// are about is where the playbook's own steps landed. The flush points have a test of their
/// own, `each_section_ends_with_a_flush_point_that_nothing_lists`.
fn structural(step: &Step) -> bool {
    matches!(step.kind, StepKind::Flush { explicit: false })
}

fn names(c: &Compiled) -> Vec<&str> {
    c.steps
        .iter()
        .filter(|s| !structural(s))
        .map(|s| s.task.name.as_str())
        .collect()
}

/// The index of the step with this name, which is not its position in [`names`] once a play
/// has a flush point in the middle of it.
fn index_of(c: &Compiled, name: &str) -> usize {
    c.steps
        .iter()
        .position(|s| s.task.name == name)
        .unwrap_or_else(|| panic!("no step named {name}"))
}

/// The order a host walks a compiled play from a given step, on the happy path.
fn walk(c: &Compiled, from: usize) -> Vec<&str> {
    let mut pos = from;
    let mut seen = Vec::new();
    while pos < c.steps.len() {
        if !structural(&c.steps[pos]) {
            seen.push(c.steps[pos].task.name.as_str());
        }
        pos = after(c, pos);
    }
    seen
}

/// The order a host walks the cleanup after the step at `failed` has failed.
fn walk_failure(c: &Compiled, failed: usize) -> Vec<&str> {
    let mut seen = Vec::new();
    let mut at = after_failure(c, failed);
    while let Some((pos, cleanup)) = at {
        if !structural(&c.steps[pos]) {
            seen.push(c.steps[pos].task.name.as_str());
        }
        at = after_pending(c, pos, cleanup);
    }
    seen
}

const NESTED: &str = r#"
- hosts: all
  tasks:
    - name: before
      command: "true"
    - block:
        - name: body one
          command: "true"
        - block:
            - name: nested body
              command: "true"
          rescue:
            - name: nested rescue
              command: "true"
          always:
            - name: nested always
              command: "true"
      rescue:
        - name: outer rescue
          command: "true"
      always:
        - name: outer always
          command: "true"
    - name: after
      command: "true"
"#;

/// The tree flattens in playbook order, section by section, and every block's span covers
/// exactly the steps it holds.
#[test]
fn a_nested_play_flattens_in_playbook_order() {
    let c = compiled(NESTED);
    assert_eq!(
        names(&c),
        [
            "before",
            "body one",
            "nested body",
            "nested rescue",
            "nested always",
            "outer rescue",
            "outer always",
            "after",
        ]
    );
    // The outer block is reserved first, so it is block 0 and the nested one is block 1.
    let outer = &c.blocks[0];
    let nested = &c.blocks[1];
    assert_eq!((outer.body.start, outer.body.end), (1, 5));
    assert_eq!((outer.rescue.start, outer.rescue.end), (5, 6));
    assert_eq!((outer.always.start, outer.always.end), (6, 7));
    assert_eq!(outer.parent, None);
    assert_eq!((nested.body.start, nested.body.end), (2, 3));
    assert_eq!((nested.rescue.start, nested.rescue.end), (3, 4));
    assert_eq!((nested.always.start, nested.always.end), (4, 5));
    assert_eq!(nested.parent, Some(0));
    assert_eq!(nested.section, Section::Body);
    for span in &c.blocks {
        assert!(span.always.end <= c.steps.len());
        assert_eq!(span.body.end, span.rescue.start);
        assert_eq!(span.rescue.end, span.always.start);
    }
}

/// How a host walks a compiled play: with no failure, after a failure at a step, and where
/// a rescue takes it.
#[rstest]
#[case::nested(
    NESTED,
    &["before", "body one", "nested body", "nested always", "outer always", "after"],
    &[("nested body", &["nested always", "outer always"][..])],
    &[
        ("body one", Some("outer rescue")),
        ("nested body", Some("nested rescue")),
        ("nested rescue", Some("outer rescue")),
        ("nested always", Some("outer rescue")),
        ("outer rescue", None),
        ("outer always", None),
        ("before", None),
    ]
)]
#[case::block_ending_where_its_parent_body_ends(
    r#"
- hosts: all
  tasks:
    - block:
        - block:
            - name: inner only
              command: "true"
      rescue:
        - name: outer rescue
          command: "true"
      always:
        - name: outer always
          command: "true"
    - name: after
      command: "true"
"#,
    &["inner only", "outer always", "after"],
    &[],
    &[]
)]
#[case::bare_block_and_task(
    "- hosts: all\n  tasks:\n    - block:\n        - name: one\n          command: \"true\"\n    - name: two\n      command: \"true\"\n",
    &["one", "two"],
    &[],
    &[]
)]
#[case::empty_rescue_leads_into_always(
    "- hosts: all\n  tasks:\n    - block:\n        - name: one\n          command: \"true\"\n      always:\n        - name: cleanup\n          command: \"true\"\n",
    &["one", "cleanup"],
    &[],
    &[]
)]
#[case::failure_inside_an_always_ends_the_section(
    r#"
- hosts: all
  tasks:
    - block:
        - name: body
          command: "true"
      always:
        - name: first cleanup
          command: "true"
        - name: second cleanup
          command: "true"
"#,
    &["body", "first cleanup", "second cleanup"],
    &[
        ("body", &["first cleanup", "second cleanup"][..]),
        ("first cleanup", &[][..]),
    ],
    &[]
)]
#[case::cleanup_written_as_a_block(
    r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - block:
            - name: cleanup one
              command: "true"
            - name: cleanup two
              command: "true"
        - name: cleanup last
          command: "true"
"#,
    &["fails", "cleanup one", "cleanup two", "cleanup last"],
    &[
        ("fails", &["cleanup one", "cleanup two", "cleanup last"][..]),
        ("cleanup one", &[][..]),
    ],
    &[]
)]
#[case::cleanup_block_with_its_own_always(
    r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - block:
            - name: cleanup one
              command: "true"
          always:
            - name: cleanup inner cleanup
              command: "true"
        - name: cleanup last
          command: "true"
"#,
    &["fails", "cleanup one", "cleanup inner cleanup", "cleanup last"],
    &[("fails", &["cleanup one", "cleanup inner cleanup", "cleanup last"][..])],
    &[]
)]
#[case::cleanup_does_not_start_in_a_rescue(
    r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - block: []
          rescue:
            - name: cleanup recovery
              command: "true"
        - name: cleanup
          command: "true"
"#,
    &["fails", "cleanup"],
    &[("fails", &["cleanup"][..])],
    &[]
)]
#[case::an_empty_body_is_not_a_way_into_a_rescue(
    r#"
- hosts: all
  tasks:
    - block: []
      rescue:
        - name: first recovery
          command: "true"
    - name: one
      command: "true"
    - block: []
      rescue:
        - name: second recovery
          command: "true"
      always:
        - name: cleanup
          command: "true"
    - name: two
      command: "true"
"#,
    &["one", "cleanup", "two"],
    &[],
    &[]
)]
#[case::no_rescue_cleans_up_and_lets_the_failure_stand(
    r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - name: cleanup
          command: "true"
    - name: after
      command: "true"
"#,
    &["fails", "cleanup", "after"],
    &[("fails", &["cleanup"][..])],
    &[("fails", None)]
)]
#[case::a_block_inside_a_rescue(
    r#"
- hosts: all
  tasks:
    - block:
        - name: body fails
          command: "true"
      rescue:
        - block:
            - name: in the rescue
              command: "true"
          rescue:
            - name: rescue of the rescue
              command: "true"
          always:
            - name: cleanup in the rescue
              command: "true"
      always:
        - name: outer cleanup
          command: "true"
"#,
    &["body fails", "outer cleanup"],
    &[("in the rescue", &["cleanup in the rescue", "outer cleanup"][..])],
    &[
        ("body fails", Some("in the rescue")),
        ("in the rescue", Some("rescue of the rescue")),
    ]
)]
#[case::rescue_target_seeks_past_a_nested_rescue(
    r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      rescue:
        - block: []
          rescue:
            - name: rescue recovery
              command: "true"
        - name: rescue cleanup
          command: "true"
      always:
        - name: outer cleanup
          command: "true"
"#,
    &["fails", "outer cleanup"],
    &[],
    &[("fails", Some("rescue cleanup"))]
)]
fn a_host_walks_blocks_as_the_reference_does(
    #[case] text: &str,
    #[case] happy: &[&str],
    #[case] failed: &[(&str, &[&str])],
    #[case] rescued: &[(&str, Option<&str>)],
) {
    let c = compiled(text);
    assert_eq!(walk(&c, first(&c)), happy, "no failure");
    for (step, cleanup) in failed {
        assert_eq!(
            walk_failure(&c, index_of(&c, step)),
            *cleanup,
            "failure at {step}"
        );
    }
    for (step, target) in rescued {
        let got = rescue_target(&c, index_of(&c, step)).map(|i| c.steps[i].task.name.as_str());
        assert_eq!(got, *target, "rescue of a failure at {step}");
    }
}

/// Every keyword a block carries reaches the tasks under it, and a task that says something
/// itself keeps its own answer.
#[test]
fn a_block_s_keywords_reach_the_tasks_under_it() {
    let c = compiled(
        r#"
- hosts: all
  tasks:
    - name: outer
      block:
        - name: inherits
          command: "true"
        - name: overrides
          command: "true"
          ignore_errors: false
          become: false
          vars: {shared: task}
          when: inner
      when: outer
      become: true
      become_user: deploy
      ignore_errors: true
      timeout: 7
      vars: {shared: block, only: block}
"#,
    );
    let inherits = &c.steps[0].task;
    assert_eq!(inherits.when, ["outer"]);
    assert_eq!(inherits.r#become, Some(true));
    assert_eq!(inherits.become_user.as_deref(), Some("deploy"));
    assert!(inherits.ignores_errors());
    assert_eq!(inherits.timeout, Some(7));
    assert_eq!(inherits.vars["shared"], serde_json::json!("block"));
    assert_eq!(inherits.vars["only"], serde_json::json!("block"));

    let overrides = &c.steps[1].task;
    assert_eq!(
        overrides.when,
        ["outer", "inner"],
        "the outer condition is evaluated first, so it is the one a skip reports"
    );
    assert_eq!(overrides.r#become, Some(false));
    assert!(
        !overrides.ignores_errors(),
        "measured: a task's own 'ignore_errors: false' beats the block's 'true'"
    );
    assert_eq!(overrides.vars["shared"], serde_json::json!("task"));
    assert_eq!(overrides.vars["only"], serde_json::json!("block"));
    // The block's name belongs to no step: measured, it is shown nowhere.
    assert_eq!(names(&c), ["inherits", "overrides"]);
}

/// `remote_user`, `port` and `connection` come down play, block, task, the innermost first.
#[test]
fn the_connection_keywords_come_down_to_the_task() {
    let c = compiled(
        r#"
- hosts: all
  remote_user: play
  port: 1
  connection: local
  tasks:
    - block:
        - name: inherits
          command: "true"
        - name: overrides
          command: "true"
          port: 3
      remote_user: block
"#,
    );
    let connect = |step: usize| {
        let c = &c.steps[step].task.connect;
        (c.remote_user.clone(), c.port.clone(), c.connection.clone())
    };
    let local = Some("local".to_string());
    assert_eq!(
        connect(0),
        (Some("block".into()), Some("1".into()), local.clone())
    );
    assert_eq!(connect(1), (Some("block".into()), Some("3".into()), local));
}

/// A `meta` task compiles into a step of its own, so nothing tries to send it to a host.
#[test]
fn meta_compiles_into_a_step_of_its_own() {
    let c = compiled("- hosts: all\n  tasks:\n    - meta: noop\n    - command: \"true\"\n");
    assert_eq!(c.steps[0].kind, StepKind::Meta);
    assert_eq!(meta_action(&c.steps[0].task), "noop");
    assert_eq!(c.steps[1].kind, StepKind::Task);
}

/// The measurement playbook the tag algebra was read off, task for task.
const TAGGED: &str = r"
- hosts: all
  tasks:
    - name: a
      debug: msg=a
      tags: [x]
    - name: b
      debug: msg=b
      tags: y
    - name: c-always
      debug: msg=c
      tags: always
    - name: d-never
      debug: msg=d
      tags: never
    - block:
        - name: e-in-block
          debug: msg=e
      tags: [blk]
    - name: f-untagged
      debug: msg=f
";

/// The selections the `--list-tags` goldens do not measure: `--tags all`, `--skip-tags never`,
/// and the literal word `untagged` beside a real tag, which does not make a task untagged.
#[test]
fn the_tag_selections_the_listing_goldens_leave_out() {
    let all_but_never = ["a", "b", "c-always", "e-in-block", "f-untagged"];
    for (run, skip) in [(&["all"][..], &[][..]), (&[][..], &["never"][..])] {
        let c = selected(TAGGED, &selection(run, skip));
        assert_eq!(
            names(&c),
            all_but_never,
            "--tags {run:?} --skip-tags {skip:?}"
        );
    }
    let sel = selection(&["tagged"], &[]);
    assert!(sel.selects(&["untagged".to_string(), "real".to_string()]));
    assert!(!sel.selects(&["untagged".to_string()]));
}

/// A block whose tasks the selection all drops leaves a span with nothing in it, and the spans
/// of everything around it still cover exactly the steps they hold.
#[test]
fn filtering_leaves_every_span_covering_its_own_steps() {
    let c = selected(
        r"
- hosts: all
  tasks:
    - name: kept before
      debug: msg=a
      tags: [keep]
    - block:
        - name: dropped
          debug: msg=b
      always:
        - name: kept cleanup
          debug: msg=c
          tags: [keep]
      tags: [drop]
    - name: kept after
      debug: msg=d
      tags: [keep]
",
        &selection(&["keep"], &[]),
    );
    assert_eq!(names(&c), ["kept before", "kept cleanup", "kept after"]);
    let span = &c.blocks[0];
    assert_eq!(span.body, 1..1, "the body lost its only task");
    assert_eq!(span.rescue, 1..1);
    assert_eq!(span.always, 1..2);
    assert_eq!(walk(&c, 0), ["kept before", "kept cleanup", "kept after"]);
}

/// The names of the steps [`expansion`] finds at the bottom.
fn brought_in(dir: &Path, play: &str, sel: &TagSelection) -> Option<Vec<String>> {
    let expanded = expansion(dir, play, sel)?.unwrap();
    Some(names(&expanded).into_iter().map(String::from).collect())
}

/// What the include statements of a play bring in under a tag selection, followed down
/// through every include they bring in themselves: the expansion at the bottom, or `None`
/// when the selection dropped the first statement.
fn expansion(dir: &Path, play: &str, sel: &TagSelection) -> Option<anyhow::Result<Compiled>> {
    let mut pb = parse(play, dir.join("x.yml").to_str().unwrap()).unwrap();
    pb.plays[0].gather_facts = false;
    let search = RoleSearch {
        paths: vec![dir.join("roles")],
        collections: Vec::new(),
    };
    let mut base = compile(&pb.plays[0], &search, sel).unwrap();
    let include = |c: &Compiled| {
        c.steps
            .iter()
            .find(|s| matches!(s.kind, StepKind::Include(_)))
            .cloned()
    };
    let mut parent = include(&base)?;
    loop {
        let target = match parent.kind {
            StepKind::Include(IncludeKind::Tasks) => {
                let name = parent.task.args["_raw_params"].as_str().unwrap();
                IncludeTarget::File(beside_or_in_role(
                    &parent.origin.file_dir,
                    parent.origin.role_dir.as_deref(),
                    name,
                ))
            }
            _ => IncludeTarget::Role(Box::new(include_role_entry(&parent.task.args).unwrap())),
        };
        let request = IncludeRequest {
            target,
            what: String::new(),
            vars: IncludeParams::default(),
            label: None,
        };
        let expanded = match expand_include(&base, &parent, &request) {
            Ok(expanded) => expanded,
            Err(err) => return Some(Err(err)),
        };
        match include(&expanded) {
            Some(next) => (parent, base) = (next, expanded),
            None => return Some(Ok(expanded)),
        }
    }
}

/// The tags the layers around a dynamic include gave the statement reach the tasks it brings
/// in, and the tags written on the statement itself stop there.
#[test]
fn a_dynamic_include_hands_down_what_the_layers_above_it_gave_it() {
    // The play, `--tags`, `--skip-tags`, and what the include at the bottom brings in.
    type Row = (
        String,
        &'static [&'static str],
        &'static [&'static str],
        Option<&'static [&'static str]>,
    );
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    for role in ["r", "r2", "r3", "nest"] {
        std::fs::create_dir_all(dir.join("roles").join(role).join("tasks")).unwrap();
    }
    let write = |path: &str, text: &str| std::fs::write(dir.join(path), text).unwrap();
    write("inc.yml", "- name: in\n  debug: msg=x\n");
    write("roles/r/tasks/main.yml", "- include_tasks: inc.yml\n");
    write("roles/r/tasks/inc.yml", "- name: in\n  debug: msg=x\n");
    write("roles/r2/tasks/main.yml", "- name: in\n  debug: msg=x\n");
    write("roles/r3/tasks/main.yml", "- include_role: {name: r2}\n");
    // Two includes deep: what the role gave the first statement has to reach the second.
    write("roles/nest/tasks/main.yml", "- include_tasks: mid.yml\n");
    write("roles/nest/tasks/mid.yml", "- include_tasks: inc.yml\n");
    write("roles/nest/tasks/inc.yml", "- name: in\n  debug: msg=x\n");
    let inc = dir.join("inc.yml");
    let inc = inc.display();
    let roles = |entry: &str| format!("- hosts: all\n  roles:\n    - {entry}\n");
    let tasks = |task: &str| format!("- hosts: all\n  tasks:\n    - {task}\n");
    let rows: [Row; 12] = [
        (roles("{role: r, tags: [t]}"), &["t"], &[], Some(&["in"])),
        (roles("{role: nest, tags: [t]}"), &["t"], &[], Some(&["in"])),
        (roles("{role: r, tags: [t]}"), &[], &["t"], None),
        (roles("{role: r, tags: [t]}"), &["zz"], &[], None),
        (
            tasks(&format!("block: [include_tasks: {inc}]\n      tags: [b]")),
            &["b"],
            &[],
            Some(&["in"]),
        ),
        (
            tasks("block: [include_role: {name: r2}]\n      tags: [b]"),
            &["b"],
            &[],
            Some(&["in"]),
        ),
        (roles("{role: r3, tags: [t]}"), &["t"], &[], Some(&["in"])),
        (
            roles("{role: r, tags: [always]}"),
            &["other"],
            &[],
            Some(&["in"]),
        ),
        (
            roles("{role: r, tags: [never, n]}"),
            &["n"],
            &[],
            Some(&["in"]),
        ),
        (roles("{role: r, tags: [never]}"), &[], &[], None),
        (
            tasks(&format!("include_tasks: {inc}\n      tags: [i]")),
            &["i"],
            &[],
            Some(&[]),
        ),
        (
            tasks("include_role: {name: r2}\n      tags: [i]"),
            &["i"],
            &[],
            Some(&[]),
        ),
    ];
    for (play, run, skip, want) in &rows {
        let got = brought_in(&dir, play, &selection(run, skip));
        let want = want.map(|w| w.iter().map(ToString::to_string).collect::<Vec<_>>());
        assert_eq!(got, want, "--tags {run:?} --skip-tags {skip:?}\n{play}");
    }

    // Every other keyword the layers around the statement give it comes down the same way.
    let all = selection(&[], &[]);
    let first = |play: &str| {
        let expanded = expansion(&dir, play, &all).unwrap();
        expanded.unwrap_or_else(|e| panic!("{e:#}")).steps[0]
            .task
            .clone()
    };
    let under_role = first(&roles("{role: r, become: true}"));
    assert_eq!(under_role.r#become, Some(true), "the role entry's become");
    let under_block = first(&tasks(&format!(
        "block: [include_tasks: {inc}]\n      no_log: true\n      when: gate"
    )));
    assert_eq!(under_block.no_log, Some(true), "the block's no_log");
    assert_eq!(under_block.when, ["gate"], "the block's when");
    let under_block = first(&tasks(&format!(
        "block: [include_tasks: {inc}]\n      check_mode: true\n      diff: false"
    )));
    assert_eq!(
        (under_block.check_mode, under_block.diff),
        (
            Some(crate::playbook::Flag::Fixed(true)),
            Some(crate::playbook::Flag::Fixed(false))
        ),
        "the block's check_mode and diff"
    );
}

/// Inserting steps one past the last step of a block's body grows that body and shifts the two
/// sections behind it, and inserting past the block moves nothing.
#[test]
fn a_splice_grows_the_section_it_lands_at_the_end_of() {
    let mut c = compiled(
        r#"
- hosts: all
  tasks:
    - block:
        - name: body one
          command: "true"
        - name: body two
          command: "true"
      rescue:
        - name: recovery
          command: "true"
      always:
        - name: cleanup
          command: "true"
    - name: after
      command: "true"
"#,
    );
    assert_eq!((c.blocks[0].body.start, c.blocks[0].body.end), (0, 2));
    assert_eq!((c.blocks[0].rescue.start, c.blocks[0].rescue.end), (2, 3));
    assert_eq!((c.blocks[0].always.start, c.blocks[0].always.end), (3, 4));
    let step = |name: &str| Step {
        kind: StepKind::Task,
        task: PlayTask {
            name: name.to_string(),
            module: "command".to_string(),
            ..PlayTask::empty()
        },
        block: Some(0),
        section: Section::Body,
        role: None,
        origin: Arc::default(),
        include_params: None,
        hosts: None,
    };
    c.splice(2, vec![step("spliced one"), step("spliced two")]);
    assert_eq!(
        names(&c),
        [
            "body one",
            "body two",
            "spliced one",
            "spliced two",
            "recovery",
            "cleanup",
            "after",
        ]
    );
    assert_eq!((c.blocks[0].body.start, c.blocks[0].body.end), (0, 4));
    assert_eq!((c.blocks[0].rescue.start, c.blocks[0].rescue.end), (4, 5));
    assert_eq!((c.blocks[0].always.start, c.blocks[0].always.end), (5, 6));
    // A host that failed in the body is still taken by the block's own rescue and not by
    // the steps that were just put in front of it.
    assert_eq!(
        rescue_target(&c, 0).map(|i| c.steps[i].task.name.clone()),
        Some("recovery".to_string())
    );
    // Past the block, nothing moves. "Past" means past the step behind it: splicing at the
    // index one after the last step of a section is splicing **into** that section, which is
    // what the `>=` above is for and what a flush written last in a body asks for.
    let before = c.blocks[0].clone();
    c.splice(7, vec![step("at the end")]);
    assert_eq!(c.blocks[0].body, before.body);
    assert_eq!(c.blocks[0].rescue, before.rescue);
    assert_eq!(c.blocks[0].always, before.always);
}

/// A notify takes the first handler of a name and every listener; a block's notify reaches
/// its tasks.
#[test]
fn a_notify_takes_the_first_handler_of_that_name_and_every_listener() {
    let c = compiled(
        r#"
- hosts: all
  handlers:
    - name: twice
      debug: msg=one
    - name: twice
      debug: msg=two
    - name: listening
      debug: msg=three
      listen: twice
    - name: h
      debug: msg=h
    - name: own
      debug: msg=own
  tasks:
    - command: "true"
      notify: twice
    - block:
        - name: inherits
          command: "true"
        - name: adds its own
          command: "true"
          notify: own
      notify: h
"#,
    );
    assert_eq!(c.handlers.len(), 5);
    let names: Vec<Option<String>> = c
        .handlers
        .iter()
        .map(|h| Some(h.task.name.clone()))
        .collect();
    assert_eq!(resolve_notify(&c, "twice", &names), [0, 2]);
    assert_eq!(resolve_notify(&c, "listening", &names), [2]);
    assert!(resolve_notify(&c, "nobody", &names).is_empty());
    assert_eq!(c.steps[1].task.notify, ["h"]);
    assert_eq!(c.steps[2].task.notify, ["own", "h"]);
}

/// A play that gathers facts opens with an unlisted `setup` step, kept by `always` tags.
#[test]
fn a_play_that_gathers_facts_opens_with_the_setup_module() {
    let text = "- hosts: all\n  tasks:\n    - name: body\n      command: \"true\"\n";
    let pb = play(text);
    let gathering = |selection: &TagSelection| {
        compile(&pb.plays[0], &RoleSearch::default(), selection).expect("a compiled play")
    };
    let c = gathering(&selection(&[], &[]));
    assert!(c.gathers, "the play asked for facts");
    assert_eq!(c.steps[0].task.module, "ansible.legacy.setup");
    assert_eq!(c.steps[0].task.name, "Gathering Facts");
    assert_eq!(c.steps[0].kind, StepKind::Task);
    assert!(!listed(&c, 0), "the reference lists no gather step");
    assert!(
        c.steps[0].task.run_once.is_none(),
        "facts are the host's own, so the step never runs once for the batch"
    );

    let tagged = gathering(&selection(&["deploy"], &[]));
    assert!(tagged.gathers, "`always` survives a tag selection");
    let skipped = gathering(&selection(&[], &["always"]));
    assert!(!skipped.gathers, "`--skip-tags always` drops the gather");
    assert_ne!(skipped.steps[0].task.module, "ansible.legacy.setup");

    let c = compiled(text);
    assert!(!c.gathers, "`gather_facts: false` gathers nothing");
    assert_eq!(c.steps[0].task.name, "body");
}

/// The three flush points close the three sections, and neither they nor a handler are listed
/// anywhere.
#[test]
fn each_section_ends_with_a_flush_point_that_nothing_lists() {
    let c = compiled(
        r#"
- hosts: all
  pre_tasks:
    - name: pre
      command: "true"
  tasks:
    - name: body
      command: "true"
    - meta: flush_handlers
  post_tasks:
    - name: post
      command: "true"
"#,
    );
    let kinds: Vec<StepKind> = c.steps.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        [
            StepKind::Task,
            StepKind::Flush { explicit: false },
            StepKind::Task,
            StepKind::Flush { explicit: true },
            StepKind::Flush { explicit: false },
            StepKind::Task,
            StepKind::Flush { explicit: false },
        ]
    );
    let listed: Vec<&str> = (0..c.steps.len())
        .filter(|&i| listed(&c, i))
        .map(|i| c.steps[i].task.name.as_str())
        .collect();
    assert_eq!(listed, ["pre", "body", "meta", "post"]);
}

/// The action table is the reference's own list, sorted and free of duplicates.
#[test]
fn the_meta_actions_are_sorted_and_free_of_duplicates() {
    let names: Vec<&str> = META_ACTIONS.to_vec();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(names, sorted);
}

/// `public` is judged by what it is worth, not by being written.
#[test]
fn public_is_refused_by_value_and_not_by_presence() {
    let asked = |value: Value| {
        let mut args = Map::new();
        args.insert("name".into(), json!("base"));
        args.insert("public".into(), value);
        include_role_options(&args)
    };
    assert!(asked(json!(false)).is_ok());
    assert!(asked(json!("no")).is_ok());
    assert!(asked(json!(true)).is_err());
    assert!(asked(json!("yes")).is_err());
    // Not knowing is not the same as being told no: an export this release cannot do is
    // refused rather than guessed at.
    assert!(asked(json!("{{ maybe }}")).is_err());
}
