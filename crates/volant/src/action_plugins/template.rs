// SPDX-License-Identifier: GPL-3.0-or-later
//! `template`: a template rendered on the controller, then copied the way `copy` copies.
//!
//! Read off `plugins/action/template.py` of ansible-core 2.19.12: the plugin checks its
//! arguments, finds the template under `templates/`, renders it once with the task's variables
//! and four of its own, and hands the text to the `copy` action with its own options taken out.
//!
//! The template's text is author content read on the controller. What it reads from a managed
//! host lands in the file as text: `Templar::render_file` renders once, and nothing renders the
//! result again.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::copy::{CopyOf, copy_bytes, failing, local_mode};
use super::files::{not_found, refuse_host_named, search_paths};
use super::{Context, Plugin};
use crate::executor::as_bool_value;
use crate::template::{FileRender, Vars};
use crate::vars::HostVars;

/// The delimiters Jinja reads, with the default each one has. A template written for others is
/// refused rather than rendered as if it used these.
const DELIMITERS: &[(&str, &str)] = &[
    ("variable_start_string", "{{"),
    ("variable_end_string", "}}"),
    ("block_start_string", "{%"),
    ("block_end_string", "%}"),
    ("comment_start_string", "{#"),
    ("comment_end_string", "#}"),
];

/// The template's own options, which the reference takes out before `copy` sees the arguments.
const TEMPLATE_ONLY: &[&str] = &[
    "newline_sequence",
    "variable_start_string",
    "variable_end_string",
    "block_start_string",
    "block_end_string",
    "comment_start_string",
    "comment_end_string",
    "trim_blocks",
    "lstrip_blocks",
    "output_encoding",
];

pub(super) fn start(ctx: Context<'_>) -> Box<dyn Plugin> {
    match rendered(&ctx) {
        Ok(of) => copy_bytes(of),
        Err(msg) => failing(msg),
    }
}

/// A string argument, `None` when absent or null. The reference turns anything else into a
/// string before it reads it.
fn string_arg(args: &Map<String, Value>, name: &str) -> Option<String> {
    match args.get(name)? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn flag(args: &Map<String, Value>, name: &str, default: bool) -> bool {
    args.get(name).and_then(as_bool_value).unwrap_or(default)
}

/// A copy of the item's variables, its provenance kept, with the four the plugin adds.
///
/// `ansible_managed`, `template_path` and `template_fullpath` are the controller's own words: a
/// `src` whose render read a host is refused before this is reached, so the path found cannot be
/// one a host chose. `template_destpath` is the `dest` as rendered, and stays a host's value when
/// its render read one.
fn template_vars(
    item_vars: &HostVars,
    args_untrusted: &BTreeSet<String>,
    src: &str,
    fullpath: &str,
    dest: &str,
) -> HostVars {
    let mut vars = item_vars.clone();
    if vars.get("ansible_managed").is_none() {
        vars.insert("ansible_managed".into(), Value::from("Ansible managed"));
    }
    vars.insert("template_path".into(), Value::from(src));
    vars.insert("template_fullpath".into(), Value::from(fullpath));
    if args_untrusted.contains("dest") {
        vars.insert_untrusted("template_destpath".into(), Value::from(dest));
    } else {
        vars.insert("template_destpath".into(), Value::from(dest));
    }
    vars
}

/// The rendered template and the arguments `copy` gets, or the task's failure.
fn rendered(ctx: &Context<'_>) -> Result<CopyOf, String> {
    let args = ctx.args;
    // The reference's checks, in its order and in its words.
    if string_arg(args, "state").is_some() {
        return Err("'state' cannot be specified on a template".into());
    }
    let (Some(src), Some(dest)) = (string_arg(args, "src"), string_arg(args, "dest")) else {
        return Err("src and dest are required".into());
    };
    // The escaped spellings are what a YAML file usually carries.
    let newline_sequence = match string_arg(args, "newline_sequence").as_deref() {
        None | Some("\n" | "\\n") => "\n",
        Some("\r" | "\\r") => "\r",
        Some("\r\n" | "\\r\\n") => "\r\n",
        Some(_) => return Err("newline_sequence needs to be one of: \n, \r or \r\n".into()),
    };
    if let Some((name, _)) = DELIMITERS
        .iter()
        .find(|(name, default)| string_arg(args, name).is_some_and(|v| v != *default))
    {
        return Err(format!(
            "argument '{name}' is not supported yet on 'template'"
        ));
    }
    if let Some(encoding) = string_arg(args, "output_encoding").filter(|e| !e.is_empty())
        && !matches!(
            encoding.to_ascii_lowercase().replace('_', "-").as_str(),
            "utf-8" | "utf8"
        )
    {
        return Err(format!(
            "output_encoding '{encoding}' is not supported yet on 'template': only utf-8 is"
        ));
    }
    refuse_host_named(ctx.args_untrusted, "src")?;
    let searched = search_paths(ctx.origin, ctx.playbook_dir, "templates", &src);
    let Some(found) = searched.iter().find(|p| p.exists()) else {
        return Err(not_found(&src, &searched));
    };
    let bytes = std::fs::read(found)
        .map_err(|err| format!("could not read src={}: {err}", found.display()))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("could not read src={} as utf-8", found.display()))?;

    let vars = template_vars(
        ctx.item_vars,
        ctx.args_untrusted,
        &src,
        &found.display().to_string(),
        &dest,
    );
    let options = FileRender {
        trim_blocks: flag(args, "trim_blocks", true),
        lstrip_blocks: flag(args, "lstrip_blocks", false),
        newline_sequence: newline_sequence.into(),
        name: Some(src.clone()),
    };
    let out = ctx
        .templar
        .render_file(&text, Vars::from(&vars), &options)
        .map_err(|err| format!("could not render {src}: {err}"))?;

    let mut args: Map<String, Value> = args
        .iter()
        .filter(|(k, _)| !TEMPLATE_ONLY.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if args.get("mode").and_then(Value::as_str) == Some("preserve") {
        args.insert("mode".into(), Value::String(local_mode(found)?));
    }
    Ok(CopyOf {
        bytes: out.into_bytes(),
        basename: found
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        args,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use volant_protocol::TaskResult;
    use volant_protocol::encoding::b64_decode;

    use super::*;
    use crate::action_plugins::files::MAX_FILE_LEN;
    use crate::action_plugins::{Step, Sub};

    /// A scratch playbook directory, removed when the test ends, a failed assertion included.
    struct Scratch(PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A playbook directory of this test process's own, holding `templates/<name>` with `text`.
    fn with_template(dir: &str, name: &str, text: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("volant-template-{}-{dir}", std::process::id()));
        std::fs::create_dir_all(dir.join("templates")).unwrap();
        std::fs::write(dir.join("templates").join(name), text).unwrap();
        Scratch(dir)
    }

    /// The four variables the plugin adds are the controller's, except a `dest` whose render
    /// read a managed host, which stays that host's value; and the item's own provenance is kept.
    ///
    /// What would make this red: `template_destpath` inserted trusted whatever `dest` read, which
    /// hands a host's string to any later render as author content, or the clone losing the
    /// item's untrusted names.
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
        let args = map(args);
        let running = Map::new();
        let templar = crate::template::Templar::new(dir.to_path_buf());
        let origin = crate::compile::Origin {
            file_dir: dir.to_path_buf(),
            ..crate::compile::Origin::default()
        };
        let mut warnings = Vec::new();
        start(Context {
            args: &args,
            args_untrusted: untrusted,
            running_vars: &running,
            delegated: false,
            escalated: false,
            item_vars,
            templar: &templar,
            origin: &origin,
            playbook_dir: dir,
            warnings: &mut warnings,
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
            Step::Done(result) => panic!("done before `copy`: {result:?}"),
        }
    }

    /// The bytes a sub-task stages, as text.
    fn staged(sub: &Sub) -> String {
        assert_eq!(sub.module, "copy");
        assert_eq!(sub.files.len(), 1, "{:?}", sub.files);
        String::from_utf8(b64_decode(&sub.files[0].1.b64).unwrap()).unwrap()
    }

    /// The task's result when it ends before any sub-task.
    fn refused(plugin: &mut dyn Plugin) -> String {
        match plugin.next(None) {
            Step::Done(result) => {
                assert!(result.failed(), "{result:?}");
                result.0["msg"].as_str().unwrap().to_string()
            }
            Step::Run(sub) => panic!("a sub-task was sent: {sub:?}"),
        }
    }

    /// What a managed host wrote reaches the file it is rendered into as text.
    ///
    /// The fact is set the way a registered result or a gathered fact is, as untrusted, and read
    /// through the host's real variables. Unix only: the lookup it carries is a `touch`.
    ///
    /// What would make this red: the template rendered through `Templar::render`, whose extra
    /// passes run the host's string on the controller; or the rendered bytes rendered once more
    /// on their way into the blob. The control is what keeps the marker's absence from passing
    /// for a `lookup` that does nothing here: the same lookup written by the template's author
    /// does run.
    #[cfg(unix)]
    #[test]
    fn a_host_value_reaches_the_rendered_file_as_text() {
        let dir = with_template("trust", "motd.j2", "banner: {{ motd }}\n");
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
    ///
    /// Measured on ansible-core 2.19.12: a `template` whose `src` is a registered `stdout` sends
    /// the controller file it names, rendered, to the host. This refuses it, on purpose.
    ///
    /// What would make this red: the refusal made after the search, which answers "Could not
    /// find or access" for the path that names nothing, or dropped, which renders the one that
    /// exists.
    #[test]
    fn a_template_a_host_named_is_never_read() {
        let dir = with_template("host-named", "secret.j2", "secret\n");
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

    const OPTS: &str = "top\n  {% if true %}\n  in\n  {% endif %}\npath={{ template_path }} dest={{ template_destpath }}\nfull={{ template_fullpath }}\n";

    /// The module's options reach the render, and the four variables the plugin sets are the
    /// reference's.
    ///
    /// Measured on ansible-core 2.19.12 with this template, in `cat -A`: `trim_blocks` on and
    /// `lstrip_blocks` off by default, the final newline kept, `newline_sequence` applied to the
    /// whole output, `template_path` the `src` as written, `template_destpath` the `dest` and
    /// `template_fullpath` the path found.
    ///
    /// What would make this red: an option read and dropped on its way to the render, the
    /// escaped spelling of `newline_sequence` taken as the four characters it is written with,
    /// or a variable set from the wrong path.
    #[test]
    fn the_options_and_variables_are_the_reference_s() {
        let dir = with_template("opts", "opts.j2", OPTS);
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

    /// `ansible_managed` is the reference's default unless the variable is already there.
    ///
    /// What would make this red: the plugin's value written over one the playbook set, which is
    /// what the reference refrains from doing.
    #[test]
    fn ansible_managed_is_set_only_when_absent() {
        let dir = with_template("managed", "m.j2", "{{ ansible_managed }}\n");
        let args = json!({"src": "m.j2", "dest": "/tmp/m"});
        let mut plugin = start_plain(&dir, args.clone());
        assert_eq!(staged(&sent(plugin.as_mut())), "Ansible managed\n");

        let mut vars = HostVars::default();
        vars.insert("ansible_managed".into(), json!("managed by hand"));
        let mut plugin = start_in(&dir, args, &BTreeSet::new(), &vars);
        assert_eq!(staged(&sent(plugin.as_mut())), "managed by hand\n");
    }

    /// The final sub-task is `copy`, named after the template, without the template's own
    /// options, and `mode: preserve` is the template file's mode.
    ///
    /// What would make this red: `trim_blocks` or `newline_sequence` handed to `copy`, which
    /// refuses an argument it does not know; the mode left as `preserve`, which the module would
    /// read off the staged copy; or `_original_basename` taken from anything but the template,
    /// which names the file wrongly under a directory `dest`.
    #[cfg(unix)]
    #[test]
    fn copy_gets_the_template_s_name_and_none_of_its_options() {
        use std::os::unix::fs::PermissionsExt;
        let dir = with_template("handoff", "opts.j2", "x\n");
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
    ///
    /// What would make this red: a check dropped, which runs `stat` for a task that cannot
    /// succeed; a delimiter accepted and ignored, which renders a template written for other
    /// delimiters as if it were not one; or the search's refusal given the prefix `copy` puts
    /// on it, which the reference's `template` does not.
    #[test]
    fn what_cannot_be_rendered_is_refused_before_the_host_is_asked() {
        let dir = with_template("refused", "t.j2", "x\n");
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
                json!({"src": "t.j2", "dest": "/x", "output_encoding": "latin-1"}),
                "output_encoding 'latin-1' is not supported yet on 'template': only utf-8 is"
                    .into(),
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
        for name in [
            "variable_start_string",
            "variable_end_string",
            "block_start_string",
            "block_end_string",
            "comment_start_string",
            "comment_end_string",
        ] {
            let mut args = map(json!({"src": "t.j2", "dest": "/x"}));
            args.insert(name.into(), json!("<<"));
            cases.push((
                Value::Object(args),
                format!("argument '{name}' is not supported yet on 'template'"),
            ));
        }
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

    /// A render too big for one frame is refused, naming both sizes, before anything is put on
    /// the host.
    ///
    /// What would make this red: the limit not applied to the rendered bytes, which puts a frame
    /// on the wire the agent refuses to read and ends the link.
    #[test]
    fn a_render_bigger_than_a_frame_is_refused() {
        let len = MAX_FILE_LEN + 1;
        let dir = with_template("big", "big.j2", &"x".repeat(len));
        let mut plugin = start_plain(&dir, json!({"src": "big.j2", "dest": "/x"}));
        plugin.next(None);
        let absent = TaskResult(map(json!({"stat": {"exists": false}})));
        let Step::Done(result) = plugin.next(Some(absent)) else {
            panic!("the render was sent")
        };
        assert_eq!(
            result.0["msg"],
            json!(format!(
                "big.j2 is {len} bytes; one frame carries at most {}",
                MAX_FILE_LEN
            ))
        );
    }
}
