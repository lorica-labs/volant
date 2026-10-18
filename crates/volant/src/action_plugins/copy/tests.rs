// SPDX-License-Identifier: GPL-3.0-or-later
use crate::testing::scratch;
use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::*;
use crate::action_plugins::{Context, Plugin, Step};

/// The plugin as the driver starts it, for arguments `prepare` rendered, the names among them
/// whose render read a managed host, and a playbook in `playbook_dir`.
fn start_in(
    args: &Map<String, Value>,
    untrusted: &BTreeSet<String>,
    playbook_dir: &Path,
) -> Box<dyn Plugin> {
    let running = crate::vars::HostVars::default();
    let item_vars = crate::vars::HostVars::default();
    let templar = crate::template::Templar::new(playbook_dir.to_path_buf());
    let origin = crate::compile::Origin {
        file_dir: playbook_dir.to_path_buf(),
        ..crate::compile::Origin::default()
    };
    let mut warnings = Vec::new();
    start(Context {
        args,
        args_untrusted: untrusted,
        running_vars: &running,
        delegated: false,
        escalated: false,
        local: false,
        item_vars: &item_vars,
        templar: &templar,
        origin: &origin,
        playbook_dir,
        warnings: &mut warnings,

        environment: None,
        timeout: None,
        check_mode: false,
        diff: false,
    })
}

fn start_copy(args: &Map<String, Value>, untrusted: &BTreeSet<String>) -> Box<dyn Plugin> {
    start_in(args, untrusted, &std::env::temp_dir())
}

/// A `src` whose render read a managed host is refused before anything is read.
#[test]
fn a_source_a_host_named_is_never_read() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let secret = dir.join("controller-secret");
    std::fs::write(&secret, "secret").unwrap();
    let untrusted: BTreeSet<String> = ["src".to_string()].into();
    for named in [secret.clone(), dir.join("no-such-file")] {
        let mut args = Map::new();
        args.insert("src".into(), json!(named.display().to_string()));
        args.insert("dest".into(), json!("/tmp/x"));
        let mut plugin = start_copy(&args, &untrusted);
        let Step::Done(result) = plugin.next(None) else {
            panic!("a sub-task was sent")
        };
        assert!(result.failed(), "{result:?}");
        assert_eq!(
            result.0["msg"],
            json!(
                "the 'src' of this task was named by a managed host, and a controller file a host chose is never sent"
            ),
            "{}",
            named.display()
        );
    }
}

/// SHA-1 of `hello\n`, as the reference's `stat` reported it for the golden `copy-new`.
const HELLO_SHA1: &str = "f572d396fae9206628714fb2ce00f72e94f2258f";

/// A playbook directory holding `files/hello.txt`, and the arguments of a task copying it.
fn hello(extra: Value) -> (tempfile::TempDir, Map<String, Value>) {
    let tmp = scratch();
    let dir = tmp.path();
    std::fs::create_dir_all(dir.join("files")).unwrap();
    std::fs::write(dir.join("files/hello.txt"), "hello\n").unwrap();
    let Value::Object(mut args) = json!({"src": "hello.txt", "dest": "/tmp/v/hello.txt"}) else {
        unreachable!()
    };
    if let Value::Object(extra) = extra {
        args.extend(extra);
    }
    (tmp, args)
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

fn copy_task(extra: Value) -> (tempfile::TempDir, Box<dyn Plugin>) {
    let (tmp, args) = hello(extra);
    let plugin = start_in(&args, &BTreeSet::new(), tmp.path());
    (tmp, plugin)
}

/// The `stat` module's own parameters, read off `ansible/modules/stat.py` of ansible-core
/// 2.19.12's `argument_spec`: an argument outside this list is one the module never declared
/// and refuses outright.
const STAT_ARGUMENT_SPEC: &[&str] = &[
    "path",
    "follow",
    "get_checksum",
    "get_mime",
    "get_attributes",
    "checksum_algorithm",
];

/// The first sub-task is the reference's `stat`, measured: the SHA-1 and nothing else, and
/// every argument sent is one the module's own `argument_spec` declares.
#[test]
fn the_stat_is_the_reference_s() {
    let (_dir, mut plugin) = copy_task(json!({}));
    let sub = run(plugin.as_mut(), None);
    assert_eq!(sub.module, "stat");
    assert_eq!(
        Value::Object(sub.args.clone()),
        json!({"path": "/tmp/v/hello.txt", "follow": false, "get_checksum": true,
               "get_mime": false, "get_attributes": false, "checksum_algorithm": "sha1"})
    );
    for key in sub.args.keys() {
        assert!(
            STAT_ARGUMENT_SPEC.contains(&key.as_str()),
            "'{key}' is not one of stat's own arguments"
        );
    }
    assert!(sub.files.is_empty());
}

/// A destination that already holds the bytes gets `file`, and nothing is sent.
#[test]
fn an_identical_file_is_not_sent() {
    let (_dir, mut plugin) = copy_task(json!({"mode": "0644"}));
    run(plugin.as_mut(), None);
    let sub = run(
        plugin.as_mut(),
        Some(
            json!({"changed": false, "stat": {"exists": true, "isdir": false, "checksum": HELLO_SHA1}}),
        ),
    );
    assert_eq!(sub.module, "file");
    assert_eq!(
        Value::Object(sub.args),
        json!({"dest": "/tmp/v/hello.txt", "_original_basename": "hello.txt",
               "recurse": false, "state": "file", "mode": "0644"})
    );
    assert!(sub.files.is_empty(), "nothing is staged: {:?}", sub.files);
    let result = done(
        plugin.as_mut(),
        json!({"changed": false, "path": "/tmp/v/hello.txt", "mode": "0644", "state": "file",
               "size": 6}),
    );
    assert_eq!(
        result,
        json!({"diff": [], "changed": false, "path": "/tmp/v/hello.txt", "mode": "0644",
               "state": "file", "size": 6, "checksum": HELLO_SHA1, "dest": "/tmp/v/hello.txt"})
    );
}

/// The staged `copy` the reference runs, measured: `src` is the staged file, the local sum
/// travels as `checksum`, and the source's name as `_original_basename`.
fn assert_sent(sub: &Sub, extra: Value) {
    assert_eq!(sub.module, "copy");
    let mut want = json!({"dest": "/tmp/v/hello.txt", "_original_basename": "hello.txt",
                          "follow": false, "checksum": HELLO_SHA1});
    if let (Value::Object(want), Value::Object(extra)) = (&mut want, extra) {
        want.extend(extra);
    }
    assert_eq!(
        Value::Object(sub.args.clone()),
        want,
        "no controller path in `src`"
    );
    assert_eq!(
        sub.files,
        [("src".to_string(), blob_of("hello.txt", b"hello\n").unwrap())]
    );
}

/// A destination that differs gets the bytes, once, and `copy`.
#[test]
fn a_different_file_is_sent_once() {
    let (_dir, mut plugin) = copy_task(json!({"backup": true}));
    run(plugin.as_mut(), None);
    let zeros = "0".repeat(40);
    let sub = run(
        plugin.as_mut(),
        Some(json!({"stat": {"exists": true, "isdir": false, "checksum": zeros}})),
    );
    assert_sent(&sub, json!({"backup": true}));
    // The module's own result, with the empty `diff` the plugin puts under it.
    let result = done(
        plugin.as_mut(),
        json!({"changed": true, "checksum": HELLO_SHA1, "dest": "/tmp/v/hello.txt",
               "src": "/staged"}),
    );
    assert_eq!(
        result,
        json!({"diff": [], "changed": true, "checksum": HELLO_SHA1,
               "dest": "/tmp/v/hello.txt", "src": "/staged"})
    );
}

/// A destination that is not there gets the bytes and `copy`, as a different one does.
#[test]
fn an_absent_file_is_sent() {
    let (_dir, mut plugin) = copy_task(json!({}));
    run(plugin.as_mut(), None);
    let sub = run(plugin.as_mut(), Some(json!({"stat": {"exists": false}})));
    assert_sent(&sub, json!({}));
}

/// `force: false` asks for no sum, and leaves a destination that exists alone.
#[test]
fn force_false_leaves_an_existing_file_alone() {
    let (tmp, args) = hello(json!({"force": false}));
    let dir = tmp.path();
    let mut plugin = start_in(&args, &BTreeSet::new(), dir);
    let sub = run(plugin.as_mut(), None);
    assert_eq!(sub.args["get_checksum"], json!(false));
    let result = done(
        plugin.as_mut(),
        json!({"stat": {"exists": true, "isdir": false}}),
    );
    assert_eq!(
        result,
        json!({"changed": false, "dest": "/tmp/v/hello.txt",
               "src": dir.join("files/hello.txt").display().to_string()})
    );
}

/// A destination that is a directory is looked at again with the source's name inside it, and
/// the module is still handed the directory.
#[test]
fn a_directory_dest_takes_the_basename() {
    let (_dir, mut plugin) = copy_task(json!({"dest": "/tmp/v"}));
    run(plugin.as_mut(), None);
    let sub = run(
        plugin.as_mut(),
        Some(json!({"stat": {"exists": true, "isdir": true}})),
    );
    assert_eq!(sub.module, "stat");
    assert_eq!(sub.args["path"], json!("/tmp/v/hello.txt"));
    let sub = run(
        plugin.as_mut(),
        Some(json!({"stat": {"exists": true, "isdir": false, "checksum": HELLO_SHA1}})),
    );
    assert_eq!(sub.module, "file");
    assert_eq!(sub.args["dest"], json!("/tmp/v"));

    // A trailing `/` says it is a directory: the name inside it is the one looked at.
    let (_dir, mut plugin) = copy_task(json!({"dest": "/tmp/v/"}));
    assert_eq!(
        run(plugin.as_mut(), None).args["path"],
        json!("/tmp/v/hello.txt")
    );
    // And when that name is itself a directory, the reference looks at the same path again:
    // `copy.py` joins the source's name to `dest`, not to the path it looked at first.
    let sub = run(
        plugin.as_mut(),
        Some(json!({"stat": {"exists": true, "isdir": true}})),
    );
    assert_eq!(sub.args["path"], json!("/tmp/v/hello.txt"));

    let mut args = Map::new();
    args.insert("content".into(), json!("x"));
    args.insert("dest".into(), json!("/tmp/v"));
    let mut plugin = start_copy(&args, &BTreeSet::new());
    run(plugin.as_mut(), None);
    let result = done(
        plugin.as_mut(),
        json!({"stat": {"exists": true, "isdir": true}}),
    );
    assert_eq!(
        result["msg"],
        json!("can not use content with a dir as dest")
    );
}

/// `remote_src` runs `copy` with the task's arguments and nothing before it, and its `src`
/// names a host file, which is the host's own to name.
#[test]
fn remote_src_runs_the_module_alone() {
    let Value::Object(args) =
        json!({"src": "/tmp/v/new.txt", "dest": "/tmp/v/remote.txt", "remote_src": true})
    else {
        unreachable!()
    };
    let mut plugin = start_copy(&args, &["src".to_string()].into());
    let sub = run(plugin.as_mut(), None);
    assert_eq!(sub.module, "copy");
    assert_eq!(sub.args, args);
    assert!(sub.files.is_empty());
    let result = done(
        plugin.as_mut(),
        json!({"changed": true, "src": "/tmp/v/new.txt"}),
    );
    assert_eq!(result, json!({"changed": true, "src": "/tmp/v/new.txt"}));
}

/// A `content` is sent as its text, a mapping as `json.dumps` writes it, and the name it
/// travels under is a dotted one the host never writes.
#[test]
fn a_content_is_sent_as_the_reference_writes_it() {
    let mut args = Map::new();
    args.insert("content".into(), json!({"b": 1, "a": ["é", true]}));
    args.insert("dest".into(), json!("/tmp/v/c.json"));
    let mut plugin = start_copy(&args, &BTreeSet::new());
    run(plugin.as_mut(), None);
    let sub = run(plugin.as_mut(), Some(json!({"stat": {"exists": false}})));
    let text = r#"{"b": 1, "a": ["\u00e9", true]}"#;
    assert_eq!(sub.files[0].1, blob_of("x", text.as_bytes()).unwrap());
    assert_eq!(sub.args["checksum"], json!(sha1_hex(text.as_bytes())));
    assert_eq!(
        sub.args["_original_basename"],
        json!(format!(".{}", &sha1_hex(text.as_bytes())[..8]))
    );
    assert!(!sub.args.contains_key("content"), "{:?}", sub.args);
}

/// What is refused before any sub-task, each in the reference's words where it has some.
#[test]
fn what_cannot_be_copied_is_refused_before_the_host_is_asked() {
    let (tmp, _) = hello(json!({}));
    let dir = tmp.path();
    std::fs::create_dir_all(dir.join("files/tree")).unwrap();
    for (args, msg) in [
        (
            json!({"dest": "/tmp/x"}),
            "src (or content) is required".to_string(),
        ),
        (json!({"src": "hello.txt"}), "dest is required".into()),
        (
            json!({"src": "hello.txt", "content": "x", "dest": "/tmp/x"}),
            "src and content are mutually exclusive".into(),
        ),
        (
            json!({"content": "x", "dest": "/tmp/x/"}),
            "can not use content with a dir as dest".into(),
        ),
        (
            json!({"src": "tree", "dest": "/tmp/x"}),
            "copying a directory is not supported yet: tree".into(),
        ),
        (
            json!({"src": "nope.txt", "dest": "/tmp/x"}),
            format!(
                "Unexpected AnsibleActionFail error: {}",
                not_found(
                    "nope.txt",
                    &[
                        dir.join("files/nope.txt"),
                        dir.join("nope.txt"),
                        dir.join("files/nope.txt"),
                        dir.join("nope.txt"),
                    ]
                )
            ),
        ),
    ] {
        let Value::Object(args) = args else {
            unreachable!()
        };
        let mut plugin = start_in(&args, &BTreeSet::new(), dir);
        let Step::Done(result) = plugin.next(None) else {
            panic!("a sub-task was sent for {args:?}")
        };
        assert!(result.failed(), "{result:?}");
        assert_eq!(result.0["msg"], json!(msg));
    }
}

/// A source too big for one frame is refused by its size, before it is read and before the host
/// is asked anything.
#[test]
fn a_source_bigger_than_a_frame_is_refused_before_it_is_read() {
    let (tmp, mut args) = hello(json!({}));
    let dir = tmp.path();
    let big = std::fs::File::create(dir.join("files/big.bin")).unwrap();
    let len = crate::action_plugins::files::MAX_FILE_LEN + 1;
    big.set_len(u64::try_from(len).unwrap()).unwrap();
    args.insert("src".into(), json!("big.bin"));
    let mut plugin = start_in(&args, &BTreeSet::new(), dir);
    let Step::Done(result) = plugin.next(None) else {
        panic!("a sub-task was sent")
    };
    assert_eq!(
        result.0["msg"],
        json!(format!(
            "big.bin is {len} bytes; one frame carries at most {}",
            crate::action_plugins::files::MAX_FILE_LEN
        ))
    );
}

/// A `stat` that fails ends the task in the reference's words, and a `copy` that fails is the
/// task's result as the module gave it.
#[test]
fn a_failed_sub_task_ends_the_task() {
    let (_dir, mut plugin) = copy_task(json!({}));
    run(plugin.as_mut(), None);
    let result = done(
        plugin.as_mut(),
        json!({"failed": true, "msg": "Permission denied"}),
    );
    assert_eq!(result["failed"], json!(true));
    assert_eq!(
        result["msg"],
        json!("Failed to get information on remote file (/tmp/v/hello.txt): Permission denied")
    );

    let (_dir, mut plugin) = copy_task(json!({"validate": "test -s %s"}));
    run(plugin.as_mut(), None);
    run(plugin.as_mut(), Some(json!({"stat": {"exists": false}})));
    let failed = json!({"changed": false, "checksum": "da39a3ee5e6b4b0d3255bfef95601890afd80709",
        "exit_status": 1, "failed": true, "msg": "failed to validate", "stderr": "",
        "stderr_lines": [], "stdout": "", "stdout_lines": []});
    let result = done(plugin.as_mut(), failed.clone());
    let Value::Object(mut want) = failed else {
        unreachable!()
    };
    want.insert("diff".into(), json!([]));
    assert_eq!(result, Value::Object(want));
}
