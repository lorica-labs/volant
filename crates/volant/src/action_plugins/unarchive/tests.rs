// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::BTreeSet;
use std::path::Path;

use serde_json::json;

use super::*;
use crate::action_plugins::files::MAX_FILE_LEN;
use crate::config::HostNamedPaths;
use crate::testing::scratch;

/// A scratch directory holding `files/` and an existing `dest/`.
fn layout() -> tempfile::TempDir {
    let tmp = scratch();
    std::fs::create_dir_all(tmp.path().join("files")).unwrap();
    std::fs::create_dir_all(tmp.path().join("dest")).unwrap();
    tmp
}

fn start_in(
    args: &Map<String, Value>,
    untrusted: &BTreeSet<String>,
    dir: &Path,
) -> Box<dyn Plugin> {
    start_mode(HostNamedPaths::Refuse, args, untrusted, dir)
}

/// [`start_in`] under the `host_named_paths` mode `mode`.
fn start_mode(
    mode: HostNamedPaths,
    args: &Map<String, Value>,
    untrusted: &BTreeSet<String>,
    dir: &Path,
) -> Box<dyn Plugin> {
    let running = crate::vars::HostVars::default();
    let item_vars = crate::vars::HostVars::default();
    let templar = crate::template::Templar::new(dir.to_path_buf());
    let origin = Origin {
        file_dir: dir.to_path_buf(),
        ..Origin::default()
    };
    let mut warnings = Vec::new();
    start(Context {
        link: &crate::action_plugins::LinkFacts::default(),
        args,
        args_untrusted: untrusted,
        running_vars: &running,
        delegated: false,
        escalated: false,
        local: false,
        item_vars: &item_vars,
        templar: &templar,
        origin: &origin,
        playbook_dir: dir,
        host_named_paths: mode,
        host_named_dest_dirs: None,
        warnings: &mut warnings,

        environment: None,
        timeout: None,
        check_mode: false,
        diff: false,
        async_val: 0,
    })
}

fn start_plain(args: &Map<String, Value>, dir: &Path) -> Box<dyn Plugin> {
    start_in(args, &BTreeSet::new(), dir)
}

fn run(plugin: &mut dyn Plugin, last: Option<Value>) -> Sub {
    let last = last.map(|v| match v {
        Value::Object(map) => TaskResult(map),
        _ => unreachable!(),
    });
    match plugin.next(last) {
        Step::Run(sub) => sub,
        other => panic!("done early: {other:?}"),
    }
}

fn done(plugin: &mut dyn Plugin, last: Value) -> Value {
    let Value::Object(last) = last else {
        unreachable!()
    };
    match plugin.next(Some(TaskResult(last))) {
        Step::Done(result) => Value::Object(result.0),
        other => panic!("another sub-task: {other:?}"),
    }
}

fn refused(plugin: &mut dyn Plugin) -> String {
    match plugin.next(None) {
        Step::Done(result) => {
            assert!(result.failed(), "{result:?}");
            result.0["msg"].as_str().unwrap().to_string()
        }
        other => panic!("a sub-task was sent: {other:?}"),
    }
}

/// Arguments unpacking an archive found under `files/` into the scratch `dest/`.
fn args(dir: &Path, extra: Value) -> Map<String, Value> {
    std::fs::write(dir.join("files/bundle.tar.gz"), b"not-really-a-tarball").unwrap();
    let Value::Object(mut args) = json!({
        "src": "bundle.tar.gz",
        "dest": dir.join("dest").display().to_string(),
    }) else {
        unreachable!()
    };
    if let Value::Object(extra) = extra {
        args.extend(extra);
    }
    args
}

/// A destination confirmed to exist and be a directory: what every case beyond the first two
/// sub-tasks needs to reach the source.
fn past_dest(plugin: &mut dyn Plugin, has_creates: bool) {
    if has_creates {
        let sub = run(plugin, None);
        assert_eq!(sub.module, "stat");
        run(plugin, Some(json!({"stat": {"exists": false}})));
    } else {
        run(plugin, None);
    }
}

/// A `src` whose render read a managed host is never read, even once the destination is
/// confirmed to be a real directory.
#[test]
fn a_source_a_host_named_is_never_read() {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    let secret = dir.join("controller-secret");
    std::fs::write(&secret, "secret").unwrap();
    let untrusted: BTreeSet<String> = ["src".to_string()].into();
    for named in [secret.clone(), dir.join("no-such-file")] {
        let mut task_args = Map::new();
        task_args.insert("src".into(), json!(named.display().to_string()));
        task_args.insert("dest".into(), json!(dir.join("dest").display().to_string()));
        let mut plugin = start_in(&task_args, &untrusted, &dir);
        run(plugin.as_mut(), None);
        let result = done(
            plugin.as_mut(),
            json!({"stat": {"exists": true, "isdir": true}}),
        );
        assert_eq!(
            result["msg"],
            json!(
                "the 'src' of this task was named by a managed host, and a controller file a host chose is never sent"
            ),
            "{}",
            named.display()
        );
    }
}

/// A host-named `src` is read as the mode says, and an archive that is read is staged for the
/// module.
#[rstest::rstest]
#[case::allow_inside(HostNamedPaths::Allow, "bundle.tar.gz", true)]
#[case::allow_climbing(HostNamedPaths::Allow, "../bundle.tar.gz", true)]
#[case::contained_inside(HostNamedPaths::Contained, "bundle.tar.gz", true)]
#[case::contained_climbing(HostNamedPaths::Contained, "../bundle.tar.gz", false)]
#[case::refuse_inside(HostNamedPaths::Refuse, "bundle.tar.gz", false)]
fn a_source_a_host_named_is_read_as_the_mode_says(
    #[case] mode: HostNamedPaths,
    #[case] name: &str,
    #[case] read: bool,
) {
    let dir_guard = layout();
    let play = dir_guard.path().join("play");
    std::fs::create_dir_all(play.join("files")).unwrap();
    std::fs::create_dir_all(play.join("dest")).unwrap();
    std::fs::write(play.join("files/bundle.tar.gz"), b"in").unwrap();
    std::fs::write(dir_guard.path().join("bundle.tar.gz"), b"out").unwrap();
    let mut task_args = Map::new();
    task_args.insert("src".into(), json!(name));
    task_args.insert(
        "dest".into(),
        json!(play.join("dest").display().to_string()),
    );
    let untrusted: BTreeSet<String> = ["src".to_string()].into();
    let mut plugin = start_mode(mode, &task_args, &untrusted, &play);
    run(plugin.as_mut(), None);
    let step = plugin.next(Some(TaskResult(
        json!({"stat": {"exists": true, "isdir": true}})
            .as_object()
            .unwrap()
            .clone(),
    )));
    match (step, read) {
        (Step::Run(sub), true) => assert_eq!(sub.module, "unarchive"),
        (Step::Done(result), false) => {
            let msg = result.0["msg"].as_str().unwrap();
            assert!(msg.contains("named by a managed host"), "{msg}");
        }
        (other, _) => panic!("{name}: read={read}, step {other:?}"),
    }
}

/// `remote_src` and `copy` are read as ansible-core reads a boolean: a source on the host is not
/// held to the directories, and the module gets it as written.
#[rstest::rstest]
#[case::remote_src_y(json!({"remote_src": "y"}), true)]
#[case::remote_src_capital_t(json!({"remote_src": "T"}), true)]
#[case::remote_src_one(json!({"remote_src": 1}), true)]
#[case::remote_src_one_point_zero(json!({"remote_src": 1.0}), true)]
#[case::copy_no(json!({"copy": "no"}), true)]
#[case::copy_n(json!({"copy": "n"}), true)]
#[case::copy_f(json!({"copy": "F"}), true)]
#[case::copy_yes(json!({"copy": "yes"}), false)]
#[case::copy_one(json!({"copy": 1}), false)]
#[case::remote_src_n(json!({"remote_src": "n"}), false)]
#[case::remote_src_one_point_zero_text(json!({"remote_src": "1.0"}), false)]
fn the_remote_source_is_read_as_ansible_core_reads_it(#[case] extra: Value, #[case] remote: bool) {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    let mut task_args = Map::new();
    task_args.insert("src".into(), json!("/nowhere/a.tgz"));
    task_args.insert("dest".into(), json!(dir.join("dest").display().to_string()));
    task_args.extend(extra.as_object().unwrap().clone());
    let untrusted: BTreeSet<String> = ["src".to_string()].into();
    let mut plugin = start_mode(HostNamedPaths::Contained, &task_args, &untrusted, &dir);
    let first = run(plugin.as_mut(), None);
    assert_eq!(first.module, "stat");
    let step = plugin.next(Some(TaskResult(
        json!({"stat": {"exists": true, "isdir": true}})
            .as_object()
            .unwrap()
            .clone(),
    )));
    match (step, remote) {
        (Step::Run(sub), true) => {
            assert_eq!(sub.module, "unarchive");
            assert_eq!(sub.args["src"], json!("/nowhere/a.tgz"));
            assert!(sub.files.is_empty());
        }
        (Step::Done(result), false) => {
            let msg = result.0["msg"].as_str().unwrap();
            assert!(msg.contains("named by a managed host"), "{msg}");
        }
        (other, _) => panic!("remote={remote}, step {other:?}"),
    }
}

/// `copy:` folds into `remote_src`, and the two together are refused before any host is asked
/// anything.
#[test]
fn copy_becomes_remote_src_and_the_two_together_are_refused() {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    let mut task_args = args(&dir, json!({"copy": false}));
    let mut plugin = start_plain(&task_args, &dir);
    run(plugin.as_mut(), None);
    let sub = run(
        plugin.as_mut(),
        Some(json!({"stat": {"exists": true, "isdir": true}})),
    );
    assert_eq!(sub.args["remote_src"], json!(true));
    assert!(!sub.args.contains_key("copy"), "{:?}", sub.args);

    task_args.insert("remote_src".into(), json!(false));
    let mut plugin = start_plain(&task_args, &dir);
    assert_eq!(
        refused(plugin.as_mut()),
        "parameters are mutually exclusive: ('copy', 'remote_src')"
    );
}

/// What is refused before any sub-task, each in the reference's words.
#[test]
fn what_cannot_be_unpacked_is_refused_before_the_host_is_asked() {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    for (extra, msg) in [
        (
            json!({"dest": null}),
            "src (or content) and dest are required",
        ),
        (
            json!({"src": null}),
            "src (or content) and dest are required",
        ),
        (
            json!({"dest": "~/there"}),
            "a '~' path is not supported yet on 'unarchive'",
        ),
        (
            json!({"creates": "~/there"}),
            "a '~' path is not supported yet on 'unarchive'",
        ),
    ] {
        let mut task_args = args(&dir, json!({}));
        if let Value::Object(extra) = extra {
            for (k, v) in extra {
                if v.is_null() {
                    task_args.remove(&k);
                } else {
                    task_args.insert(k, v);
                }
            }
        }
        let mut plugin = start_plain(&task_args, &dir);
        assert_eq!(refused(plugin.as_mut()), msg, "{task_args:?}");
    }
}

/// `creates` already there on the host skips the task before `dest` is even looked at, and
/// nothing is sent.
#[test]
fn creates_already_there_skips_before_dest_is_asked() {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    let task_args = args(&dir, json!({"creates": "/tmp/unarchive-marker"}));
    let mut plugin = start_plain(&task_args, &dir);
    let sub = run(plugin.as_mut(), None);
    assert_eq!(sub.module, "stat");
    assert_eq!(
        Value::Object(sub.args),
        json!({"path": "/tmp/unarchive-marker", "follow": true, "get_checksum": false,
               "get_mime": false, "get_attributes": false, "checksum_algorithm": "sha1"})
    );
    let result = done(
        plugin.as_mut(),
        json!({"stat": {"exists": true, "isdir": false}}),
    );
    assert_eq!(
        result,
        json!({"changed": false, "msg": "skipped, since /tmp/unarchive-marker exists",
               "skipped": true})
    );

    // A `stat` that fails is read the way the reference's shell test reads a command that
    // could not run: as "not there", not as a task failure.
    let mut plugin = start_plain(&task_args, &dir);
    run(plugin.as_mut(), None);
    let sub = run(
        plugin.as_mut(),
        Some(json!({"failed": true, "msg": "boom"})),
    );
    assert_eq!(sub.module, "stat", "creates absent, dest is asked next");
    assert_eq!(
        sub.args["path"],
        json!(dir.join("dest").display().to_string())
    );
}

/// The destination must exist and be a directory, `stat`'s own words when it fails, and the
/// reference's sentence otherwise.
#[test]
fn a_destination_that_is_not_a_directory_is_refused() {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    let task_args = args(&dir, json!({}));
    for stat_result in [
        json!({"stat": {"exists": false}}),
        json!({"stat": {"exists": true, "isdir": false}}),
    ] {
        let mut plugin = start_plain(&task_args, &dir);
        run(plugin.as_mut(), None);
        let result = done(plugin.as_mut(), stat_result);
        assert_eq!(
            result["msg"],
            json!(format!(
                "dest '{}' must be an existing dir",
                dir.join("dest").display()
            ))
        );
    }
    let mut plugin = start_plain(&task_args, &dir);
    run(plugin.as_mut(), None);
    let result = done(
        plugin.as_mut(),
        json!({"failed": true, "msg": "Permission denied"}),
    );
    assert_eq!(
        result["msg"],
        json!(format!(
            "Failed to get information on remote file ({}): Permission denied",
            dir.join("dest").display()
        ))
    );
}

/// A local source is searched, refused if a host named it, staged, and the module runs with
/// `decrypt` and `src` gone from its own arguments, the staged bytes in `files`.
#[test]
fn a_local_source_is_staged_and_sent() {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    let task_args = args(&dir, json!({"decrypt": false}));
    let mut plugin = start_plain(&task_args, &dir);
    past_dest(plugin.as_mut(), false);
    let sub = run(
        plugin.as_mut(),
        Some(json!({"stat": {"exists": true, "isdir": true}})),
    );
    assert_eq!(sub.module, "unarchive");
    assert_eq!(
        Value::Object(sub.args),
        json!({"dest": dir.join("dest").display().to_string()})
    );
    assert_eq!(
        sub.files,
        [(
            "src".to_string(),
            blob_of("bundle.tar.gz", b"not-really-a-tarball").unwrap()
        )]
    );
    let result = done(
        plugin.as_mut(),
        json!({"changed": true, "dest": dir.join("dest").display().to_string(), "state": "directory"}),
    );
    assert_eq!(
        result,
        json!({"changed": true, "dest": dir.join("dest").display().to_string(),
               "state": "directory"}),
        "the module's result travels as it is, no diff added"
    );
}

/// `remote_src` runs the module alone, with the task's own `src`, and nothing is searched, read
/// or staged.
#[test]
fn remote_src_sends_the_module_alone() {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    let mut task_args = args(&dir, json!({"remote_src": true, "decrypt": false}));
    task_args.insert("src".into(), json!("/on/the/host.tar.gz"));
    let mut plugin = start_in(&task_args, &["src".to_string()].into(), &dir);
    run(plugin.as_mut(), None);
    let sub = run(
        plugin.as_mut(),
        Some(json!({"stat": {"exists": true, "isdir": true}})),
    );
    assert_eq!(sub.module, "unarchive");
    assert_eq!(
        Value::Object(sub.args),
        json!({"src": "/on/the/host.tar.gz", "dest": dir.join("dest").display().to_string(),
               "remote_src": true})
    );
    assert!(sub.files.is_empty(), "nothing is staged: {:?}", sub.files);
}

/// A source too big for one frame is refused by its size, before it is read.
#[test]
fn a_source_bigger_than_a_frame_is_refused_before_it_is_read() {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    let mut task_args = args(&dir, json!({}));
    let big = std::fs::File::create(dir.join("files/big.tar.gz")).unwrap();
    let len = MAX_FILE_LEN + 1;
    big.set_len(u64::try_from(len).unwrap()).unwrap();
    task_args.insert("src".into(), json!("big.tar.gz"));
    let mut plugin = start_plain(&task_args, &dir);
    run(plugin.as_mut(), None);
    let result = done(
        plugin.as_mut(),
        json!({"stat": {"exists": true, "isdir": true}}),
    );
    assert_eq!(
        result["msg"],
        json!(format!(
            "big.tar.gz is {len} bytes; one frame carries at most {MAX_FILE_LEN}"
        ))
    );
}

/// A `src` that is a directory on the controller is refused by name once `dest` is confirmed,
/// and nothing is staged.
#[test]
fn a_directory_source_is_refused_by_name() {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    let mut task_args = args(&dir, json!({}));
    std::fs::create_dir_all(dir.join("files/tree")).unwrap();
    task_args.insert("src".into(), json!("tree"));
    let mut plugin = start_plain(&task_args, &dir);
    run(plugin.as_mut(), None);
    let result = done(
        plugin.as_mut(),
        json!({"stat": {"exists": true, "isdir": true}}),
    );
    assert_eq!(
        result["msg"],
        json!("src is a directory, not an archive: tree")
    );
}

/// A `src` that names nothing on the controller is refused with the reference's own wording,
/// which points the operator at `remote_src`.
#[test]
fn a_missing_source_names_where_it_looked() {
    let dir_guard = layout();
    let dir = dir_guard.path().to_path_buf();
    let mut task_args = args(&dir, json!({}));
    task_args.insert("src".into(), json!("nope.tar.gz"));
    let mut plugin = start_plain(&task_args, &dir);
    run(plugin.as_mut(), None);
    let result = done(
        plugin.as_mut(),
        json!({"stat": {"exists": true, "isdir": true}}),
    );
    let searched = search_paths(
        &Origin {
            file_dir: dir.clone(),
            ..Origin::default()
        },
        &dir,
        "files",
        "nope.tar.gz",
    );
    assert_eq!(
        result["msg"],
        json!(format!(
            "Task failed: {}\nIf you are using a module and expect the file to exist on the remote, see the remote_src option",
            not_found("nope.tar.gz", &searched)
        ))
    );
}
