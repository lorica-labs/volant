// SPDX-License-Identifier: GPL-3.0-or-later
//! Playbook loading: plays and tasks, with the keywords this release supports.

use std::path::Path;

use anyhow::{Context, anyhow, bail};
use serde_json::{Map, Value};
use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlLoader};

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
/// Modules whose free-form string is a command line, kept whole as `_raw_params`.
const RAW_PARAM_MODULES: &[&str] = &["command", "shell", "raw"];

pub fn load(path: &Path) -> anyhow::Result<Playbook> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading playbook {}", path.display()))?;
    parse(&text, &path.display().to_string())
}

pub fn parse(text: &str, source: &str) -> anyhow::Result<Playbook> {
    let docs =
        YamlLoader::load_from_str(text).with_context(|| format!("{source}: invalid YAML"))?;
    let plays = match docs.first() {
        Some(Yaml::Array(items)) => items,
        _ => bail!("{source}: a playbook must be a list of plays"),
    };
    let plays = plays
        .iter()
        .enumerate()
        .map(|(i, y)| parse_play(y).with_context(|| format!("{source}: play {}", i + 1)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Playbook { plays })
}

/// `yaml-rust2` 0.12 implements `Index<&str>`/`Index<usize>` on `Yaml` itself (returning
/// `BadValue` for a missing key), but not `Index<&Yaml>` on the `Hash` map: that indexing goes
/// through `hashlink::LinkedHashMap`'s own `Index`, which panics on a missing key like
/// `std::collections::HashMap` does. Fields are looked up with `get` and defaulted explicitly
/// instead.
fn field<'a>(map: &'a Hash, key: &str) -> &'a Yaml {
    static BAD_VALUE: Yaml = Yaml::BadValue;
    map.get(&Yaml::String(key.to_string()))
        .unwrap_or(&BAD_VALUE)
}

/// Ansible playbooks come from PyYAML, whose default bool resolver also accepts `yes`/`no`/
/// `on`/`off`; `yaml-rust2` follows the YAML 1.2 core schema and parses those as plain strings.
fn as_bool(yaml: &Yaml) -> Option<bool> {
    match yaml {
        Yaml::Boolean(b) => Some(*b),
        Yaml::String(s) => match s.as_str() {
            "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Some(true),
            "false" | "False" | "FALSE" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn parse_play(yaml: &Yaml) -> anyhow::Result<Play> {
    let map = yaml
        .as_hash()
        .ok_or_else(|| anyhow!("a play must be a mapping"))?;
    for key in map.keys() {
        let key = key.as_str().unwrap_or_default();
        if !PLAY_KEYWORDS.contains(&key) {
            bail!("play keyword '{key}' is not supported yet");
        }
    }
    let hosts = match field(map, "hosts") {
        Yaml::String(s) => s.clone(),
        Yaml::Array(items) => items
            .iter()
            .filter_map(Yaml::as_str)
            .collect::<Vec<_>>()
            .join(","),
        Yaml::BadValue => bail!("a play needs 'hosts'"),
        other => bail!("'hosts' must be a string or a list, found {other:?}"),
    };
    let name = field(map, "name").as_str().unwrap_or(&hosts).to_string();
    let gather_facts = as_bool(field(map, "gather_facts")).unwrap_or(true);
    let tasks = match field(map, "tasks") {
        Yaml::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, y)| parse_task(y).with_context(|| format!("task {}", i + 1)))
            .collect::<anyhow::Result<Vec<_>>>()?,
        Yaml::BadValue | Yaml::Null => Vec::new(),
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
        .as_hash()
        .ok_or_else(|| anyhow!("a task must be a mapping"))?;
    let name = field(map, "name").as_str().map(str::to_string);
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
    if let Yaml::Hash(extra) = field(map, "args") {
        for (k, v) in extra {
            let k = k
                .as_str()
                .ok_or_else(|| anyhow!("task '{label}': 'args' keys must be strings"))?;
            args.insert(k.to_string(), to_json(v)?);
        }
    }
    let ignore_errors = as_bool(field(map, "ignore_errors")).unwrap_or(false);
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
    let short = module.rsplit('.').next().unwrap_or(module);
    let mut args = Map::new();
    match value {
        Yaml::Hash(_) => {
            if let Value::Object(map) = to_json(value)? {
                args = map;
            }
        }
        Yaml::String(s) if RAW_PARAM_MODULES.contains(&short) => {
            args.insert("_raw_params".into(), Value::String(s.clone()));
        }
        // A deliberate divergence: YAML's core schema resolves an unquoted `true`/`false` as a
        // boolean, and ansible-playbook refuses it with "unexpected parameter type in action".
        // Volant takes the value back to the text it was written as and runs it, so
        // `command: false` runs `/bin/false` where Ansible would stop on an error.
        Yaml::Boolean(b) if RAW_PARAM_MODULES.contains(&short) => {
            args.insert("_raw_params".into(), Value::String(b.to_string()));
        }
        Yaml::String(s) => {
            for word in shlex::split(s).ok_or_else(|| anyhow!("unbalanced quotes in '{s}'"))? {
                let (k, v) = word
                    .split_once('=')
                    .ok_or_else(|| anyhow!("expected key=value, found '{word}'"))?;
                args.insert(k.to_string(), Value::String(v.to_string()));
            }
        }
        Yaml::Null => {}
        other => bail!("module arguments must be a mapping or a string, found {other:?}"),
    }
    Ok(args)
}

fn to_json(yaml: &Yaml) -> anyhow::Result<Value> {
    Ok(match yaml {
        Yaml::Real(s) => s
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::String(s.clone())),
        Yaml::Integer(i) => Value::from(*i),
        Yaml::String(s) => Value::String(s.clone()),
        Yaml::Boolean(b) => Value::Bool(*b),
        Yaml::Array(items) => {
            Value::Array(items.iter().map(to_json).collect::<anyhow::Result<_>>()?)
        }
        Yaml::Hash(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let k = k
                    .as_str()
                    .ok_or_else(|| anyhow!("mapping keys must be strings"))?;
                out.insert(k.to_string(), to_json(v)?);
            }
            Value::Object(out)
        }
        Yaml::Null | Yaml::BadValue => Value::Null,
        Yaml::Alias(_) => bail!("YAML aliases are not supported yet"),
    })
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
}
