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
    pub tasks: Vec<PlayTask>,
}

#[derive(Debug)]
pub struct PlayTask {
    pub name: String,
    pub module: String,
    pub args: Map<String, Value>,
    pub ignore_errors: bool,
}

/// Play keywords accepted in this release. Anything else is refused loudly rather than ignored.
const PLAY_KEYWORDS: &[&str] = &["name", "hosts", "gather_facts", "tasks"];
const TASK_KEYWORDS: &[&str] = &["name", "ignore_errors", "args"];

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
    Ok(PlayTask {
        name: name.unwrap_or_else(|| module.clone()),
        module,
        args,
        ignore_errors,
    })
}

/// Ansible task keywords that exist but are not handled yet. Refusing them keeps a playbook
/// from silently running without its conditions.
fn is_reserved_task_keyword(key: &str) -> bool {
    matches!(
        key,
        "when"
            | "loop"
            | "with_items"
            | "register"
            | "become"
            | "become_user"
            | "changed_when"
            | "failed_when"
            | "until"
            | "retries"
            | "delay"
            | "notify"
            | "tags"
            | "vars"
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
            | "loop_control"
            | "check_mode"
            | "diff"
            | "throttle"
            | "timeout"
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
            "- hosts: all\n  tasks:\n    - name: Later\n      command: true\n      when: x\n",
            "x.yml",
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("when"), "{text}");
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
}
