// SPDX-License-Identifier: GPL-3.0-or-later
//! Playbook loading: plays and tasks, with the keywords this release supports.

use std::path::Path;

use anyhow::{Context, anyhow, bail};
use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};
use volant_protocol::modules::native;

use crate::yaml::{as_bool, field, to_json};

#[derive(Debug, Default)]
pub struct Playbook {
    pub plays: Vec<Play>,
}

#[derive(Debug)]
pub struct Play {
    pub name: String,
    pub hosts: String,
    pub gather_facts: bool,
    pub vars: Map<String, Value>,
    pub vars_files: Vec<String>,
    pub tasks: Vec<PlayTask>,
}

#[derive(Debug)]
pub struct PlayTask {
    pub name: String,
    pub module: String,
    pub args: Map<String, Value>,
    pub ignore_errors: bool,
    pub timeout: Option<u64>,
    pub vars: Map<String, Value>,
    /// Conditions that must all hold, as Jinja2 expressions.
    pub when: Vec<String>,
    /// Raw `loop` value: a list, or a template string rendering to one. `with_items` lands
    /// here too and is flattened one level at run time.
    pub loop_items: Option<Value>,
    /// Whether `loop_items` came from `with_items` rather than `loop`: `with_items` flattens
    /// one level, `loop` does not.
    pub with_items: bool,
    pub loop_var: String,
    pub loop_label: Option<String>,
    pub register: Option<String>,
    pub changed_when: Vec<String>,
    pub failed_when: Vec<String>,
}

/// Play keywords accepted in this release. Anything else is refused loudly rather than ignored.
const PLAY_KEYWORDS: &[&str] = &[
    "name",
    "hosts",
    "gather_facts",
    "tasks",
    "vars",
    "vars_files",
];
const TASK_KEYWORDS: &[&str] = &[
    "name",
    "ignore_errors",
    "args",
    "timeout",
    "vars",
    "when",
    "loop",
    "with_items",
    "loop_control",
    "register",
    "changed_when",
    "failed_when",
];

/// Whether the module's string form is one command line rather than `key=value` pairs.
fn is_free_form(module: &str) -> bool {
    native(module).is_some_and(|m| m.free_form)
}

pub fn load(path: &Path) -> anyhow::Result<Playbook> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading playbook {}", path.display()))?;
    parse(&text, &path.display().to_string())
}

pub fn parse(text: &str, source: &str) -> anyhow::Result<Playbook> {
    let docs = crate::yaml::load(text, source)?;
    let plays = match docs.first() {
        Some(Yaml::Sequence(items)) => items,
        _ => bail!("{source}: a playbook must be a list of plays"),
    };
    let plays = plays
        .iter()
        .enumerate()
        .map(|(i, y)| parse_play(y).with_context(|| format!("{source}: play {}", i + 1)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Playbook { plays })
}

fn parse_play(yaml: &Yaml) -> anyhow::Result<Play> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a play must be a mapping"))?;
    for (key, _) in map {
        let key = key.as_str().unwrap_or_default();
        if !PLAY_KEYWORDS.contains(&key) {
            bail!("play keyword '{key}' is not supported yet");
        }
    }
    let hosts = match field(yaml, "hosts") {
        Some(Yaml::Value(Scalar::String(s))) => s.to_string(),
        Some(Yaml::Sequence(items)) => items
            .iter()
            .filter_map(Yaml::as_str)
            .collect::<Vec<_>>()
            .join(","),
        None => bail!("a play needs 'hosts'"),
        Some(other) => bail!("'hosts' must be a string or a list, found {other:?}"),
    };
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .unwrap_or(&hosts)
        .to_string();
    let gather_facts = field(yaml, "gather_facts")
        .and_then(as_bool)
        .unwrap_or(true);
    let vars = match field(yaml, "vars") {
        None | Some(Yaml::Value(Scalar::Null)) => Map::new(),
        Some(v) => match to_json(v).context("'vars'")? {
            Value::Object(map) => map,
            _ => bail!("'vars' must be a mapping"),
        },
    };
    let vars_files = match field(yaml, "vars_files") {
        None | Some(Yaml::Value(Scalar::Null)) => Vec::new(),
        Some(Yaml::Sequence(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("'vars_files' entries must be strings"))
            })
            .collect::<anyhow::Result<_>>()?,
        Some(Yaml::Value(Scalar::String(s))) => vec![s.to_string()],
        _ => bail!("'vars_files' must be a list of paths"),
    };
    let tasks = match field(yaml, "tasks") {
        Some(Yaml::Sequence(items)) => items
            .iter()
            .enumerate()
            .map(|(i, y)| parse_task(y).with_context(|| format!("task {}", i + 1)))
            .collect::<anyhow::Result<Vec<_>>>()?,
        None | Some(Yaml::Value(Scalar::Null)) => Vec::new(),
        _ => bail!("'tasks' must be a list"),
    };
    Ok(Play {
        name,
        hosts,
        gather_facts,
        vars,
        vars_files,
        tasks,
    })
}

fn parse_task(yaml: &Yaml) -> anyhow::Result<PlayTask> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a task must be a mapping"))?;
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .map(str::to_string);
    let label = name.clone().unwrap_or_else(|| "unnamed".to_string());
    let mut module: Option<(String, &Yaml)> = None;
    for (key, value) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("task '{label}': keys must be strings"))?;
        if TASK_KEYWORDS.contains(&key) {
            continue;
        }
        if key.contains('.') || !is_reserved_task_keyword(key) {
            if let Some((first, _)) = &module {
                bail!("task '{label}': two modules given, '{first}' and '{key}'");
            }
            module = Some((key.to_string(), value));
        } else {
            bail!("task '{label}': keyword '{key}' is not supported yet");
        }
    }
    let (module, value) = module.ok_or_else(|| anyhow!("task '{label}': no module given"))?;
    let mut args = module_args(&module, value).with_context(|| format!("task '{label}'"))?;
    if let Some(Yaml::Mapping(extra)) = field(yaml, "args") {
        for (k, v) in extra {
            let k = k
                .as_str()
                .ok_or_else(|| anyhow!("task '{label}': 'args' keys must be strings"))?;
            args.insert(k.to_string(), to_json(v)?);
        }
    }
    let ignore_errors = field(yaml, "ignore_errors")
        .and_then(as_bool)
        .unwrap_or(false);
    let timeout = match field(yaml, "timeout") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(Yaml::Value(Scalar::Integer(i))) if *i >= 0 => Some(*i as u64),
        Some(other) => {
            bail!("task '{label}': 'timeout' must be a non-negative integer, found {other:?}")
        }
    };
    let vars = match field(yaml, "vars") {
        None | Some(Yaml::Value(Scalar::Null)) => Map::new(),
        Some(v) => match to_json(v).with_context(|| format!("task '{label}': 'vars'"))? {
            Value::Object(map) => map,
            _ => bail!("task '{label}': 'vars' must be a mapping"),
        },
    };
    let when = conditions(yaml, "when", &label)?;
    let changed_when = conditions(yaml, "changed_when", &label)?;
    let failed_when = conditions(yaml, "failed_when", &label)?;
    let register = match field(yaml, "register") {
        None => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(_) => bail!("task '{label}': 'register' must be a variable name"),
    };
    let (loop_items, with_items) = match (field(yaml, "loop"), field(yaml, "with_items")) {
        (Some(_), Some(_)) => bail!("task '{label}': 'loop' and 'with_items' cannot both be given"),
        (Some(v), None) => (
            Some(to_json(v).with_context(|| format!("task '{label}': loop"))?),
            false,
        ),
        (None, Some(v)) => (
            Some(to_json(v).with_context(|| format!("task '{label}': with_items"))?),
            true,
        ),
        (None, None) => (None, false),
    };
    let (loop_var, loop_label) = match field(yaml, "loop_control") {
        None => ("item".to_string(), None),
        Some(control) => (
            field(control, "loop_var")
                .and_then(|v| v.as_str())
                .unwrap_or("item")
                .to_string(),
            field(control, "label")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        ),
    };
    Ok(PlayTask {
        name: name.unwrap_or_else(|| module.clone()),
        module,
        args,
        ignore_errors,
        timeout,
        vars,
        when,
        loop_items,
        with_items,
        loop_var,
        loop_label,
        register,
        changed_when,
        failed_when,
    })
}

/// `when`, `changed_when`, `failed_when`: one expression or a list of them. A YAML boolean is
/// spelled back as Python would (`True`/`False`) so the expression evaluator reads it.
fn conditions(yaml: &Yaml, key: &str, label: &str) -> anyhow::Result<Vec<String>> {
    let one = |node: &Yaml| -> anyhow::Result<String> {
        match node {
            Yaml::Value(Scalar::String(s)) => Ok(s.to_string()),
            Yaml::Value(Scalar::Boolean(b)) => Ok(if *b { "True" } else { "False" }.to_string()),
            other => bail!(
                "task '{label}': '{key}' must be an expression or a list of expressions, found {other:?}"
            ),
        }
    };
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Vec::new()),
        Some(Yaml::Sequence(items)) => items.iter().map(one).collect(),
        Some(node) => Ok(vec![one(node)?]),
    }
}

/// Ansible task keywords that exist but are not handled yet. Refusing them keeps a playbook
/// from silently running without its conditions.
fn is_reserved_task_keyword(key: &str) -> bool {
    matches!(
        key,
        "become"
            | "become_user"
            | "until"
            | "retries"
            | "delay"
            | "notify"
            | "tags"
            | "environment"
            | "delegate_to"
            | "run_once"
            | "no_log"
            | "block"
            | "rescue"
            | "always"
            | "include_tasks"
            | "import_tasks"
            | "include_role"
            | "import_role"
            | "check_mode"
            | "diff"
            | "throttle"
            | "any_errors_fatal"
            | "async"
            | "poll"
            | "connection"
            | "remote_user"
            | "collections"
            | "debugger"
    )
}

fn module_args(module: &str, value: &Yaml) -> anyhow::Result<Map<String, Value>> {
    let mut args = Map::new();
    match value {
        Yaml::Mapping(_) => {
            if let Value::Object(map) = to_json(value)? {
                args = map;
            }
        }
        Yaml::Value(Scalar::String(s)) if is_free_form(module) => {
            args.insert("_raw_params".into(), Value::String(s.to_string()));
        }
        // A deliberate divergence: YAML's core schema resolves an unquoted `true`/`false` as a
        // boolean, and ansible-playbook refuses it with "unexpected parameter type in action".
        // Volant takes the value back to the text it was written as and runs it, so
        // `command: false` runs `/bin/false` where Ansible would stop on an error.
        Yaml::Value(Scalar::Boolean(b)) if is_free_form(module) => {
            args.insert("_raw_params".into(), Value::String(b.to_string()));
        }
        Yaml::Value(Scalar::String(s)) => {
            for word in shlex::split(s).ok_or_else(|| anyhow!("unbalanced quotes in '{s}'"))? {
                let (k, v) = word
                    .split_once('=')
                    .ok_or_else(|| anyhow!("expected key=value, found '{word}'"))?;
                args.insert(k.to_string(), Value::String(v.to_string()));
            }
        }
        Yaml::Value(Scalar::Null) => {}
        other => bail!("module arguments must be a mapping or a string, found {other:?}"),
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
- name: Smoke test
  hosts: web,db
  gather_facts: false
  tasks:
    - name: Say hello
      command: echo hello
    - shell: echo $HOME
      ignore_errors: yes
    - name: Structured args
      ansible.builtin.command:
        cmd: ls -l
        chdir: /tmp
    - name: Key value form
      raw: uptime
    - name: Args block
      command:
        argv: [true]
      args:
        chdir: /
"#;

    #[test]
    fn plays_and_tasks_are_loaded() {
        let pb = parse(SAMPLE, "site.yml").unwrap();
        assert_eq!(pb.plays.len(), 1);
        let play = &pb.plays[0];
        assert_eq!(play.name, "Smoke test");
        assert_eq!(play.hosts, "web,db");
        assert!(!play.gather_facts);
        assert_eq!(play.tasks.len(), 5);

        let t = &play.tasks[0];
        assert_eq!(
            (t.name.as_str(), t.module.as_str()),
            ("Say hello", "command")
        );
        assert_eq!(t.args["_raw_params"], "echo hello");
        assert!(!t.ignore_errors);

        let t = &play.tasks[1];
        assert_eq!(t.name, "shell", "unnamed tasks take the module name");
        assert!(t.ignore_errors);

        let t = &play.tasks[2];
        assert_eq!(t.module, "ansible.builtin.command");
        assert_eq!(t.args["cmd"], "ls -l");
        assert_eq!(t.args["chdir"], "/tmp");

        let t = &play.tasks[4];
        assert_eq!(t.args["argv"], serde_json::json!([true]));
        assert_eq!(t.args["chdir"], "/");
    }

    #[test]
    fn hosts_may_be_a_list_and_gather_facts_defaults_to_true() {
        let pb = parse("- hosts: [web, db]\n  tasks: []\n", "x.yml").unwrap();
        assert_eq!(pb.plays[0].hosts, "web,db");
        assert_eq!(pb.plays[0].name, "web,db", "unnamed plays take the pattern");
        assert!(pb.plays[0].gather_facts);
    }

    #[test]
    fn key_value_free_form_is_parsed_for_other_modules() {
        let pb = parse(
            "- hosts: all\n  tasks:\n    - file: path=/tmp/x state=touch\n",
            "x.yml",
        )
        .unwrap();
        let t = &pb.plays[0].tasks[0];
        assert_eq!(t.args["path"], "/tmp/x");
        assert_eq!(t.args["state"], "touch");
    }

    #[test]
    fn unsupported_keywords_are_refused_with_context() {
        let err = parse(
            "- hosts: all\n  tasks:\n    - name: Later\n      command: true\n      until: x\n",
            "x.yml",
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("until"), "{text}");
        assert!(text.contains("Later"), "{text}");
        let err = parse("- hosts: all\n  become: yes\n  tasks: []\n", "x.yml").unwrap_err();
        assert!(format!("{err:#}").contains("become"));
    }

    #[test]
    fn a_task_needs_exactly_one_module() {
        let err = parse("- hosts: all\n  tasks:\n    - name: Nothing\n", "x.yml").unwrap_err();
        assert!(format!("{err:#}").contains("no module"));
        let err = parse(
            "- hosts: all\n  tasks:\n    - command: a\n      shell: b\n",
            "x.yml",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("two modules"));
    }

    #[test]
    fn an_unquoted_boolean_command_is_taken_as_its_literal_text() {
        let pb = parse("- hosts: all\n  tasks:\n    - command: false\n", "x.yml").unwrap();
        assert_eq!(pb.plays[0].tasks[0].args["_raw_params"], "false");
    }

    #[test]
    fn a_playbook_must_be_a_list_of_plays() {
        assert!(parse("hosts: all\n", "x.yml").is_err());
        assert!(parse("", "x.yml").is_err());
    }

    #[test]
    fn free_form_follows_the_module_registry() {
        let pb = parse(
            "- hosts: all\n  tasks:\n    - ansible.legacy.shell: echo a=b\n",
            "x.yml",
        )
        .unwrap();
        assert_eq!(pb.plays[0].tasks[0].args["_raw_params"], "echo a=b");
        let err = parse(
            "- hosts: all\n  tasks:\n    - community.general.command: echo a\n",
            "x.yml",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("expected key=value"),
            "another collection's command is not free-form here"
        );
    }

    #[test]
    fn a_vault_value_in_a_task_is_refused_with_context() {
        let err = parse("- hosts: all\n  tasks:\n    - name: Secret\n      command:\n        cmd: !vault |\n          $ANSIBLE_VAULT;1.1;AES256\n          3132\n", "x.yml").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("vault") && text.contains("Secret"), "{text}");
    }

    const KEYWORDS: &str = r#"
- name: Keywords
  hosts: all
  gather_facts: false
  vars:
    port: 80
    greeting: "hello {{ name }}"
  vars_files:
    - vars/common.yml
    - "vars/{{ env }}.yml"
  tasks:
    - name: Conditional
      command: echo {{ port }}
      when: port > 1
      register: out
      vars:
        local: 1
      timeout: 5
    - name: Several conditions
      command: true
      when:
        - out is defined
        - out.rc == 0
      changed_when: false
      failed_when:
        - out.rc != 0
        - "'oops' in out.stdout"
    - name: Looping
      debug:
        msg: "{{ item }}"
      loop: "{{ ['a', 'b'] }}"
    - name: Legacy loop with control
      debug:
        msg: "{{ server.name }}"
      with_items:
        - {name: a}
        - {name: b}
      loop_control:
        loop_var: server
        label: "{{ server.name }}"
    - name: Facts
      set_fact:
        computed: "{{ port + 1 }}"
"#;

    #[test]
    fn play_vars_and_vars_files_are_kept_raw() {
        let pb = parse(KEYWORDS, "k.yml").unwrap();
        let play = &pb.plays[0];
        assert_eq!(play.vars["port"], serde_json::json!(80));
        assert_eq!(play.vars["greeting"], serde_json::json!("hello {{ name }}"));
        assert_eq!(play.vars_files, ["vars/common.yml", "vars/{{ env }}.yml"]);
    }

    #[test]
    fn task_keywords_are_read() {
        let pb = parse(KEYWORDS, "k.yml").unwrap();
        let t = &pb.plays[0].tasks;
        assert_eq!(t[0].when, ["port > 1"]);
        assert_eq!(t[0].register.as_deref(), Some("out"));
        assert_eq!(t[0].vars["local"], serde_json::json!(1));
        assert_eq!(t[0].timeout, Some(5));
        assert_eq!(
            t[0].args["_raw_params"], "echo {{ port }}",
            "arguments stay untemplated here"
        );

        assert_eq!(t[1].when, ["out is defined", "out.rc == 0"]);
        assert_eq!(t[1].changed_when, ["False"]);
        assert_eq!(t[1].failed_when, ["out.rc != 0", "'oops' in out.stdout"]);

        assert_eq!(t[2].loop_items, Some(serde_json::json!("{{ ['a', 'b'] }}")));
        assert_eq!(t[2].loop_var, "item");
        assert!(t[2].loop_label.is_none());

        assert_eq!(
            t[3].loop_items,
            Some(serde_json::json!([{"name": "a"}, {"name": "b"}]))
        );
        assert_eq!(t[3].loop_var, "server");
        assert_eq!(t[3].loop_label.as_deref(), Some("{{ server.name }}"));
        assert!(t[3].with_items && !t[2].with_items);

        assert_eq!(t[4].module, "set_fact");
        assert_eq!(t[4].args["computed"], "{{ port + 1 }}");
    }

    #[test]
    fn when_accepts_booleans_and_refuses_other_scalars() {
        let pb = parse(
            "- hosts: all\n  tasks:\n    - command: true\n      when: false\n",
            "x.yml",
        )
        .unwrap();
        assert_eq!(pb.plays[0].tasks[0].when, ["False"]);
        let err = parse(
            "- hosts: all\n  tasks:\n    - command: true\n      when: 3\n",
            "x.yml",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("when"));
    }

    #[test]
    fn loop_and_with_items_together_are_refused() {
        let err = parse(
            "- hosts: all\n  tasks:\n    - debug:\n      loop: [1]\n      with_items: [2]\n",
            "x.yml",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("loop"));
    }

    #[test]
    fn still_unsupported_keywords_are_refused() {
        for kw in ["become", "until", "notify", "block", "delegate_to"] {
            let err = parse(
                &format!("- hosts: all\n  tasks:\n    - command: true\n      {kw}: x\n"),
                "x.yml",
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains(kw), "{kw}");
        }
    }
}
