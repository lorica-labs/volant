// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::Path;

use serde_json::json;
use volant_protocol::TaskResult;
use volant_protocol::encoding::b64_decode;

use super::*;
use crate::action_plugins::{Step, Sub};
use crate::config::HostNamedPaths;
use crate::testing::{scratch, write};

struct Scratch(tempfile::TempDir);

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.0.path()
    }
}

/// A playbook directory holding `templates/<name>` with `text`.
fn with_template(name: &str, text: &str) -> Scratch {
    let dir = scratch();
    write(dir.path(), &format!("templates/{name}"), text);
    Scratch(dir)
}

/// The four variables the plugin adds are the controller's, except a `dest` whose render read a
/// managed host, which stays that host's value; and the item's own provenance is kept.
#[test]
fn a_dest_a_host_named_stays_the_host_s_value() {
    let mut item = HostVars::default();
    item.insert_untrusted("motd".into(), json!("from a host"));
    let untrusted: BTreeSet<String> = ["dest".to_string()].into();
    let vars = template_vars(&item, &untrusted, "t.j2", "/pb/templates/t.j2", "/tmp/x");
    let names: Vec<&str> = vars.untrusted.iter().map(String::as_str).collect();
    assert_eq!(names, ["motd", "template_destpath"]);
    assert_eq!(vars.get("template_destpath"), Some(&json!("/tmp/x")));

    let vars = template_vars(&item, &BTreeSet::new(), "t.j2", "/pb/templates/t.j2", "/x");
    let names: Vec<&str> = vars.untrusted.iter().map(String::as_str).collect();
    assert_eq!(
        names,
        ["motd"],
        "a `dest` the playbook wrote is the author's"
    );
}

/// A `src` whose render read a managed host makes `template_path` and `template_fullpath` that
/// host's values, as a `dest` does for `template_destpath`.
#[test]
fn a_src_a_host_named_stays_the_host_s_value() {
    let item = HostVars::default();
    let untrusted: BTreeSet<String> = ["src".to_string()].into();
    let vars = template_vars(&item, &untrusted, "t.j2", "/pb/templates/t.j2", "/tmp/x");
    let names: Vec<&str> = vars.untrusted.iter().map(String::as_str).collect();
    assert_eq!(names, ["template_fullpath", "template_path"]);
    assert_eq!(vars.get("template_path"), Some(&json!("t.j2")));
    assert_eq!(
        vars.get("template_fullpath"),
        Some(&json!("/pb/templates/t.j2"))
    );
    assert!(
        template_vars(&item, &BTreeSet::new(), "t.j2", "/pb/templates/t.j2", "/x")
            .untrusted
            .is_empty(),
        "a `src` the playbook wrote is the author's"
    );
}

fn map(value: Value) -> Map<String, Value> {
    let Value::Object(map) = value else {
        unreachable!()
    };
    map
}

/// The plugin as the driver starts it, for a task written in the playbook in `dir`.
fn start_in(
    dir: &Path,
    args: Value,
    untrusted: &BTreeSet<String>,
    item_vars: &HostVars,
) -> Box<dyn Plugin> {
    start_mode(HostNamedPaths::Refuse, dir, args, untrusted, item_vars)
}

/// [`start_in`] under the `host_named_paths` mode `mode`.
fn start_mode(
    mode: HostNamedPaths,
    dir: &Path,
    args: Value,
    untrusted: &BTreeSet<String>,
    item_vars: &HostVars,
) -> Box<dyn Plugin> {
    let args = map(args);
    let running = HostVars::default();
    let templar = crate::template::Templar::new(dir.to_path_buf());
    let origin = crate::compile::Origin {
        file_dir: dir.to_path_buf(),
        ..crate::compile::Origin::default()
    };
    let mut warnings = Vec::new();
    start(Context {
        link: &crate::action_plugins::LinkFacts::default(),
        args: &args,
        args_untrusted: untrusted,
        running_vars: &running,
        delegated: false,
        escalated: false,
        local: false,
        item_vars,
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

fn start_plain(dir: &Path, args: Value) -> Box<dyn Plugin> {
    start_in(dir, args, &BTreeSet::new(), &HostVars::default())
}

/// The `copy` sub-task the plugin ends up sending for a destination that is not there.
fn sent(plugin: &mut dyn Plugin) -> Sub {
    let Step::Run(stat) = plugin.next(None) else {
        panic!("no `stat` first")
    };
    assert_eq!(stat.module, "stat");
    let absent = TaskResult(map(json!({"stat": {"exists": false}})));
    match plugin.next(Some(absent)) {
        Step::Run(sub) => sub,
        other => panic!("done before `copy`: {other:?}"),
    }
}

/// The bytes a sub-task stages.
fn staged_bytes(sub: &Sub) -> Vec<u8> {
    assert_eq!(sub.module, "copy");
    assert_eq!(sub.files.len(), 1, "{:?}", sub.files);
    b64_decode(&sub.files[0].1.b64).unwrap()
}

/// The bytes a sub-task stages, as text.
fn staged(sub: &Sub) -> String {
    String::from_utf8(staged_bytes(sub)).unwrap()
}

/// The task's result when it ends before any sub-task.
fn refused(plugin: &mut dyn Plugin) -> String {
    match plugin.next(None) {
        Step::Done(result) => {
            assert!(result.failed(), "{result:?}");
            result.0["msg"].as_str().unwrap().to_string()
        }
        other => panic!("a sub-task was sent: {other:?}"),
    }
}

/// What a managed host wrote reaches the file it is rendered into as text.
#[cfg(unix)]
#[test]
fn a_host_value_reaches_the_rendered_file_as_text() {
    let dir = with_template("motd.j2", "banner: {{ motd }}\n");
    let marker = dir.join("marker");
    let _ = std::fs::remove_file(&marker);
    let payload = format!("{{{{ lookup('pipe', 'touch {}') }}}}", marker.display());

    let inventory = crate::inventory::Inventory::parse_ini("h1\n").unwrap();
    let mut store =
        crate::vars::VarStore::new(&inventory, None, Path::new("."), Map::new()).unwrap();
    store.set_untrusted_fact("h1", "motd", Value::String(payload.clone()));
    let scope = crate::vars::Scope::default();
    let vars = HostVars {
        map: store.for_host("h1", &scope),
        untrusted: store.untrusted_of("h1", &scope),
        untrusted_hosts: store.untrusted_hosts(),
        ..HostVars::default()
    };
    let args = json!({"src": "motd.j2", "dest": "/tmp/motd"});
    let mut plugin = start_in(&dir, args, &BTreeSet::new(), &vars);
    assert_eq!(
        staged(&sent(plugin.as_mut())),
        format!("banner: {payload}\n")
    );
    assert!(!marker.exists(), "the host's lookup ran on the controller");

    std::fs::write(dir.join("templates/author.j2"), format!("{payload}\n")).unwrap();
    let mut plugin = start_plain(&dir, json!({"src": "author.j2", "dest": "/tmp/motd"}));
    sent(plugin.as_mut());
    assert!(marker.exists(), "the author's own lookup runs");
    let _ = std::fs::remove_file(&marker);
}

/// A `src` whose render read a managed host is refused before anything is looked up.
#[test]
fn a_template_a_host_named_is_never_read() {
    let dir = with_template("secret.j2", "secret\n");
    let untrusted: BTreeSet<String> = ["src".to_string()].into();
    for named in [dir.join("templates/secret.j2"), dir.join("no-such.j2")] {
        let args = json!({"src": named.display().to_string(), "dest": "/tmp/x"});
        let mut plugin = start_in(&dir, args, &untrusted, &HostVars::default());
        assert_eq!(
            refused(plugin.as_mut()),
            "the 'src' of this task was named by a managed host, and a controller file a host chose is never sent",
            "{}",
            named.display()
        );
    }
}

/// A `src` a managed host named is read as the mode says.
#[rstest::rstest]
#[case::allow(HostNamedPaths::Allow, true)]
#[case::contained(HostNamedPaths::Contained, true)]
#[case::refuse(HostNamedPaths::Refuse, false)]
fn a_template_a_host_named_is_read_as_the_mode_says(
    #[case] mode: HostNamedPaths,
    #[case] read: bool,
) {
    let dir = with_template("motd.j2", "path={{ template_path }}\n");
    let untrusted: BTreeSet<String> = ["src".to_string()].into();
    let args = json!({"src": "motd.j2", "dest": "/tmp/x"});
    let mut plugin = start_mode(mode, &dir, args, &untrusted, &HostVars::default());
    if read {
        assert_eq!(staged(&sent(plugin.as_mut())), "path=motd.j2\n");
    } else {
        assert!(refused(plugin.as_mut()).contains("named by a managed host"));
    }
}

/// Under `contained`, a template that leaves the playbook's directories is refused.
#[test]
fn a_template_a_host_named_outside_the_directories_is_refused_when_contained() {
    let dir = with_template("in.j2", "in\n");
    let outside = scratch();
    write(outside.path(), "out.j2", "out\n");
    let untrusted: BTreeSet<String> = ["src".to_string()].into();
    let named = outside.path().join("out.j2").display().to_string();
    for src in [named.as_str(), "../out.j2"] {
        let args = json!({"src": src, "dest": "/tmp/x"});
        let mut plugin = start_mode(
            HostNamedPaths::Contained,
            &dir,
            args,
            &untrusted,
            &HostVars::default(),
        );
        assert_eq!(
            refused(plugin.as_mut()),
            format!(
                "the 'src' of this task was named by a managed host, and '{src}' is not inside the files or templates directories of the playbook or the role (host_named_paths = contained)"
            )
        );
    }
}

/// A delimiter whose value came from a managed host is taken under `allow`, as ansible-core takes
/// it, and refused under the stricter modes.
#[rstest::rstest]
#[case::allow(HostNamedPaths::Allow, false)]
#[case::contained(HostNamedPaths::Contained, true)]
#[case::refuse(HostNamedPaths::Refuse, true)]
fn a_delimiter_a_host_chose_is_read_as_the_mode_says(
    #[case] mode: HostNamedPaths,
    #[case] refuse: bool,
) {
    let dir = with_template(
        "t.j2",
        "x [[ 1 ]]
",
    );
    let untrusted: BTreeSet<String> = ["variable_start_string".to_string()].into();
    let args = json!({"src": "t.j2", "dest": "/tmp/x", "variable_start_string": "[[", "variable_end_string": "]]"});
    let mut plugin = start_mode(mode, &dir, args, &untrusted, &HostVars::default());
    if refuse {
        assert_eq!(
            refused(plugin.as_mut()),
            "the 'variable_start_string' of this task was named by a managed host, and a delimiter a host chose is never used"
        );
    } else {
        assert_eq!(
            staged(&sent(plugin.as_mut())),
            "x 1
"
        );
    }
}

/// A `#jinja2:` line in a file a host named is read when the file is in the author's directory.
#[rstest::rstest]
#[case::allow(HostNamedPaths::Allow)]
#[case::contained(HostNamedPaths::Contained)]
fn a_header_in_a_file_a_host_named_is_read(#[case] mode: HostNamedPaths) {
    let dir = with_template(
        "t.j2",
        "#jinja2: variable_start_string:'[[', variable_end_string:']]'
x [[ 1 ]]
",
    );
    let untrusted: BTreeSet<String> = ["src".to_string()].into();
    let args = json!({"src": "t.j2", "dest": "/tmp/x"});
    let mut plugin = start_mode(mode, &dir, args, &untrusted, &HostVars::default());
    assert_eq!(
        staged(&sent(plugin.as_mut())),
        "x 1
"
    );
}

const OPTS: &str = "top\n  {% if true %}\n  in\n  {% endif %}\npath={{ template_path }} dest={{ template_destpath }}\nfull={{ template_fullpath }}\n";

/// The module's options reach the render, and the four variables the plugin sets are the
/// reference's.
#[test]
fn the_options_and_variables_are_the_reference_s() {
    let dir = with_template("opts.j2", OPTS);
    let full = dir.join("templates/opts.j2").display().to_string();
    let tail = |dest: &str| format!("path=opts.j2 dest={dest}\nfull={full}\n");
    for (extra, want) in [
        (json!({}), format!("top\n    in\n  {}", tail("/tmp/o"))),
        (
            json!({"lstrip_blocks": true}),
            format!("top\n  in\n{}", tail("/tmp/o")),
        ),
        (
            json!({"trim_blocks": false}),
            format!("top\n  \n  in\n  \n{}", tail("/tmp/o")),
        ),
        (
            json!({"newline_sequence": "\\r\\n"}),
            format!("top\n    in\n  {}", tail("/tmp/o")).replace('\n', "\r\n"),
        ),
    ] {
        let mut args = map(json!({"src": "opts.j2", "dest": "/tmp/o"}));
        args.extend(map(extra.clone()));
        let mut plugin = start_plain(&dir, Value::Object(args));
        assert_eq!(staged(&sent(plugin.as_mut())), want, "{extra}");
    }
}

/// The delimiters and the encoding of a task reach the file, each row as `ansible-playbook`
/// of ansible-core 2.19.12 wrote it.
#[test]
fn the_delimiters_and_the_encoding_reach_the_file() {
    let dir = with_template(
        "d.j2",
        "[[ template_path ]] {{ one }} <% if 1 %>b<% endif %>\ncaf\u{e9}\n",
    );
    let at = |extra: Value| {
        let mut args = map(json!({"src": "d.j2", "dest": "/tmp/d"}));
        args.extend(map(extra));
        let mut plugin = start_plain(&dir, Value::Object(args));
        staged_bytes(&sent(plugin.as_mut()))
    };
    let vs = json!({"variable_start_string": "[[", "variable_end_string": "]]"});
    assert_eq!(
        at(vs.clone()),
        "d.j2 {{ one }} <% if 1 %>b<% endif %>\ncaf\u{e9}\n".as_bytes()
    );
    let mut both = map(vs.clone());
    both.extend(map(
        json!({"block_start_string": "<%", "block_end_string": "%>"}),
    ));
    assert_eq!(
        at(Value::Object(both.clone())),
        "d.j2 {{ one }} bcaf\u{e9}\n".as_bytes()
    );
    both.insert("output_encoding".into(), json!("latin-1"));
    assert_eq!(
        at(Value::Object(both.clone())),
        b"d.j2 {{ one }} bcaf\xe9\n"
    );
    both.insert("output_encoding".into(), json!("utf-16-le"));
    let utf16: Vec<u8> = "d.j2 {{ one }} bcaf\u{e9}\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert_eq!(at(Value::Object(both)), utf16);
    // A text with none of the start strings is written as it is, even with an encoding.
    assert_eq!(
        at(
            json!({"variable_start_string": "@@", "block_start_string": "@%", "comment_start_string": "@#"})
        ),
        "[[ template_path ]] {{ one }} <% if 1 %>b<% endif %>\ncaf\u{e9}\n".as_bytes()
    );
}

/// `ansible_managed` is the reference's default unless the variable is already there.
#[test]
fn ansible_managed_is_set_only_when_absent() {
    let dir = with_template("m.j2", "{{ ansible_managed }}\n");
    let args = json!({"src": "m.j2", "dest": "/tmp/m"});
    let mut plugin = start_plain(&dir, args.clone());
    assert_eq!(staged(&sent(plugin.as_mut())), "Ansible managed\n");

    let mut vars = HostVars::default();
    vars.insert("ansible_managed".into(), json!("managed by hand"));
    let mut plugin = start_in(&dir, args, &BTreeSet::new(), &vars);
    assert_eq!(staged(&sent(plugin.as_mut())), "managed by hand\n");
}

/// The final sub-task is `copy`, named after the template, without the template's own options,
/// and `mode: preserve` is the template file's mode.
#[cfg(unix)]
#[test]
fn copy_gets_the_template_s_name_and_none_of_its_options() {
    use std::os::unix::fs::PermissionsExt;
    let dir = with_template("opts.j2", "x\n");
    let path = dir.join("templates/opts.j2");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let args = json!({"src": "opts.j2", "dest": "/tmp/o", "mode": "preserve", "owner": "root",
        "trim_blocks": true, "lstrip_blocks": false, "newline_sequence": "\\n",
        "output_encoding": "utf-8", "variable_start_string": "{{"});
    let mut plugin = start_plain(&dir, args);
    let sub = sent(plugin.as_mut());
    assert_eq!(
        Value::Object(sub.args),
        json!({"dest": "/tmp/o", "mode": "0640", "owner": "root", "follow": false,
               "_original_basename": "opts.j2",
               "checksum": volant_protocol::encoding::sha1_hex(b"x\n")})
    );
}

/// What is refused before any sub-task, each in the reference's words where it has some.
#[test]
#[expect(clippy::too_many_lines)]
fn what_cannot_be_rendered_is_refused_before_the_host_is_asked() {
    let dir = with_template("t.j2", "x\n");
    std::fs::write(dir.join("templates/undefined.j2"), "{{ nope }}\n").unwrap();
    std::fs::write(
        dir.join("templates/include.j2"),
        "{% include 'other.j2' %}\n",
    )
    .unwrap();
    std::fs::write(dir.join("templates/latin.j2"), b"x\xe9\n").unwrap();
    let searched = search_paths(
        &crate::compile::Origin {
            file_dir: dir.to_path_buf(),
            ..crate::compile::Origin::default()
        },
        &dir,
        "templates",
        "nope.j2",
    );
    let mut cases = vec![
        (
            json!({"src": "t.j2", "dest": "/x", "state": "file"}),
            "'state' cannot be specified on a template".to_string(),
        ),
        (json!({"dest": "/x"}), "src and dest are required".into()),
        (json!({"src": "t.j2"}), "src and dest are required".into()),
        (
            json!({"src": "t.j2", "dest": "/x", "newline_sequence": "x"}),
            "newline_sequence needs to be one of: \n, \r or \r\n".into(),
        ),
        (
            json!({"src": "t.j2", "dest": "/x", "output_encoding": "koi8-r"}),
            "output_encoding 'koi8-r' is not supported yet on 'template': only utf-8, utf-8-sig, latin-1, ascii, cp1252, iso-8859-15, utf-16 and utf-32 are".into(),
        ),
        (
            json!({"src": "nope.j2", "dest": "/x"}),
            not_found("nope.j2", &searched),
        ),
        (
            json!({"src": "latin.j2", "dest": "/x"}),
            format!(
                "could not read src={} as utf-8",
                dir.join("templates/latin.j2").display()
            ),
        ),
    ];
    // The reference reads an empty or null string as none, and its overrides refuse none; two
    // start strings that are one are refused too, a list is not a string, and a number is.
    for name in [
        "variable_start_string",
        "variable_end_string",
        "block_start_string",
        "block_end_string",
        "comment_start_string",
        "comment_end_string",
    ] {
        for value in [json!(""), Value::Null] {
            let mut args = map(json!({"src": "t.j2", "dest": "/x"}));
            args.insert(name.into(), value);
            cases.push((
                Value::Object(args),
                format!(
                    "TemplateOverrides.{name} must be <class 'str'> instead of <class 'NoneType'>"
                ),
            ));
        }
    }
    for extra in [
        json!({"variable_start_string": "{%"}),
        json!({"variable_start_string": "[[", "block_start_string": "[["}),
        json!({"comment_start_string": "{{"}),
        json!({"block_start_string": "{#"}),
    ] {
        let mut args = map(json!({"src": "t.j2", "dest": "/x"}));
        args.extend(map(extra));
        cases.push((
            Value::Object(args),
            "Block, variable and comment start strings must be different.".into(),
        ));
    }
    cases.push((
        json!({"src": "t.j2", "dest": "/x", "variable_start_string": "@".repeat(65)}),
        "could not render t.j2: Syntax error in template: variable_start_string must not be longer than 64 characters".into(),
    ));
    cases.push((
        json!({"src": "t.j2", "dest": "/x", "variable_start_string": ["a"]}),
        "Invalid value provided for 'string': ['a']".into(),
    ));
    for (args, msg) in cases {
        let mut plugin = start_plain(&dir, args.clone());
        assert_eq!(refused(plugin.as_mut()), msg, "{args}");
    }

    // A render that fails is the task's failure, before the host is asked anything, and
    // says what failed.
    let mut plugin = start_plain(&dir, json!({"src": "undefined.j2", "dest": "/x"}));
    let msg = refused(plugin.as_mut());
    assert!(
        msg.starts_with("could not render undefined.j2: ") && msg.contains("undefined"),
        "{msg}"
    );
    // The error names the file and its line, not minijinja's `<string>`: a template that
    // includes others has more than one place the undefined name could be.
    assert!(
        msg.contains("(in undefined.j2:") && !msg.contains("<string>"),
        "{msg}"
    );
    let mut plugin = start_plain(&dir, json!({"src": "include.j2", "dest": "/x"}));
    let msg = refused(plugin.as_mut());
    assert!(msg.contains("other.j2"), "{msg}");
}

/// A render too big for one frame is refused, naming both sizes, before anything is put on the
/// host.
#[test]
fn a_render_bigger_than_a_frame_is_refused() {
    crate::action_plugins::files::FRAME_LIMIT.set(1024);
    let len = 1025;
    let dir = with_template("big.j2", &"x".repeat(len));
    let mut plugin = start_plain(&dir, json!({"src": "big.j2", "dest": "/x"}));
    plugin.next(None);
    let absent = TaskResult(map(json!({"stat": {"exists": false}})));
    let Step::Done(result) = plugin.next(Some(absent)) else {
        panic!("the render was sent")
    };
    assert_eq!(
        result.0["msg"],
        json!(format!(
            "big.j2 is {len} bytes; one frame carries at most 1024"
        ))
    );
}
