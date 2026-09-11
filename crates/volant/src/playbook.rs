// SPDX-License-Identifier: GPL-3.0-or-later
//! Playbook loading: plays and tasks, with the keywords this release supports.

use std::path::Path;

use anyhow::{Context, anyhow, bail};
use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};
use volant_protocol::modules::{is_known, native};

use crate::keywords::{BLOCK_SECTIONS, Support, loop_control_keyword, play_keyword, task_keyword};
use crate::yaml::{as_bool, field, to_json};

#[derive(Debug, Default)]
pub struct Playbook {
    pub plays: Vec<Play>,
}

#[derive(Debug, Clone)]
pub struct Play {
    pub name: String,
    pub hosts: String,
    pub gather_facts: bool,
    pub vars: Map<String, Value>,
    pub vars_files: Vec<String>,
    pub tasks: Vec<PlayTask>,
    /// `become`, unset when the play says nothing: a task and the host variables both get to
    /// speak before the connection defaults do.
    pub r#become: Option<bool>,
    pub become_user: Option<String>,
    /// The execution strategy the play asked for, as written. Only `linear` exists here; the
    /// pre-flight refuses the others by name.
    pub strategy: Option<String>,
    /// Keywords the reference accepts and this release does not execute yet, sorted. The
    /// loader keeps the play rather than refusing it, so a playbook parses and lists the same
    /// way it does in the reference; the pre-flight then refuses the run before the first
    /// connection. Nothing may quietly drop a keyword between the two.
    pub unsupported: Vec<&'static str>,
}

#[derive(Debug, Clone)]
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
    pub r#become: Option<bool>,
    pub become_user: Option<String>,
    /// Keywords the reference accepts and this release does not execute yet, sorted. A task
    /// carrying one is loaded whole and refused by the pre-flight, never run without it.
    ///
    /// Sorted rather than left in mapping order so the refusal names the same keyword whatever
    /// order the YAML happened to yield: a task carrying two of them would otherwise blame
    /// whichever came first in the file.
    pub unsupported: Vec<&'static str>,
}

impl PlayTask {
    /// A task with nothing in it, for the one shape the loader builds without reading a
    /// module: a construct the pre-flight is about to refuse. It never reaches an executor.
    fn empty() -> Self {
        PlayTask {
            name: String::new(),
            module: String::new(),
            args: Map::new(),
            ignore_errors: false,
            timeout: None,
            vars: Map::new(),
            when: Vec::new(),
            loop_items: None,
            with_items: false,
            loop_var: "item".to_string(),
            loop_label: None,
            register: None,
            changed_when: Vec::new(),
            failed_when: Vec::new(),
            r#become: None,
            become_user: None,
            unsupported: Vec::new(),
        }
    }
}

/// The only escalation method this release implements. `su`, `doas`, `pbrun` and the rest are
/// refused by name at load time: escalating through a different program is not the same
/// operation, and quietly using `sudo` where the playbook asked for `su` would run the task
/// under rules the operator never wrote.
pub const BECOME_METHOD: &str = "sudo";

/// The three escalation keywords, wherever they appear. `become_user` and `become_method` are
/// read as text; `become` goes through `as_bool`, so `yes`, `on` and `"true"` all work as they
/// do in Ansible.
///
/// `become_method` comes back as nothing: refusing everything but `sudo` right here is all a
/// caller could ever do with it, so it is refused here and not stored.
fn escalation(yaml: &Yaml, context: &str) -> anyhow::Result<(Option<bool>, Option<String>)> {
    let text = |key: &str| -> anyhow::Result<Option<String>> {
        match field(yaml, key) {
            None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
            Some(Yaml::Value(Scalar::String(s))) => Ok(Some(s.to_string())),
            Some(other) => bail!("{context}'{key}' must be a name, found {other:?}"),
        }
    };
    let flag = boolean(yaml, "become")?;
    let user = text("become_user")?;
    if let Some(method) = text("become_method")?
        && method != BECOME_METHOD
    {
        // Exit 2, not the 4 the rest of a refused playbook gets: measured, the reference reads
        // this keyword happily and then fails the task that would have used it, which is 2.
        return Err(crate::stats::Refusal::at(
            2,
            format!("{context}become_method '{method}' is not supported yet"),
        ));
    }
    Ok((flag, user))
}

/// A keyword whose value has to be a boolean, refused in the reference's own words when it is
/// not one, rather than quietly becoming the default.
///
/// Measured on ansible-core 2.19.12: `gather_facts: maybe` refuses the load at exit 4 with
/// `Error processing keyword 'gather_facts': The value 'maybe' could not be converted to
/// 'bool'.`, and `ignore_errors: maybe` carries that same sentence into the task it fails at
/// run time. `become: maybe` does the same. A default silently put in their place runs the
/// playbook under a value nobody wrote and nobody is told about, which is the family of bug the
/// split between loading and refusing exists to close.
///
/// All three are refused here, at load. That is one step earlier than the reference refuses
/// `ignore_errors` and `become`, and the divergence is deliberate: no task has run yet, so
/// there is nothing half-applied to explain.
fn boolean(yaml: &Yaml, key: &str) -> anyhow::Result<Option<bool>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(node) => match as_bool(node) {
            Some(b) => Ok(Some(b)),
            None => bail!(
                "Error processing keyword '{key}': The value {} could not be converted to 'bool'.",
                shown(node)
            ),
        },
    }
}

/// A scalar the way that refusal shows it: a string in quotes, a number bare, a sequence or
/// mapping the way ansible-core's own Python repr prints it.
///
/// Measured on ansible-core 2.19.12 (`docs/superpowers/architecture.md`): a float is bare with
/// its decimal kept (`1.0`, `3.5`, never a Rust `OrderedFloat` wrapper), a sequence reads
/// `[1, 2]`, a mapping reads `{'a': 1}`, and `null` reads `None`. Ansible's own message for a
/// `null` host entry is a different sentence entirely ("Hosts list cannot contain values of
/// 'None'", not the generic invalid-value sentence this engine reuses for every shape); this
/// engine keeps the one sentence and only fixes the value rendered into it, which is this
/// function's whole job.
fn shown(node: &Yaml) -> String {
    match node {
        Yaml::Value(Scalar::String(s)) => format!("'{s}'"),
        Yaml::Value(Scalar::Integer(i)) => i.to_string(),
        Yaml::Value(Scalar::FloatingPoint(f)) => format!("{:?}", f.into_inner()),
        Yaml::Value(Scalar::Boolean(b)) => if *b { "True" } else { "False" }.to_string(),
        Yaml::Value(Scalar::Null) => "None".to_string(),
        Yaml::Sequence(_) | Yaml::Mapping(_) => {
            to_json(node).map_or_else(|_| format!("{node:?}"), |v| python_repr(&v))
        }
        other => format!("{other:?}"),
    }
}

/// A `serde_json::Value` printed the way Python's own `repr` renders it, since that is what
/// ansible-core's error messages quote a structured value with: strings single-quoted, `null` as
/// `None`, booleans capitalized, everything else bare. Used only by [`shown`], for the sequence
/// and mapping shapes `to_json` already knows how to walk.
fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{s}'"),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("'{k}': {}", python_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Whether the module's string form is one command line rather than `key=value` pairs.
fn is_free_form(module: &str) -> bool {
    native(module).is_some_and(|m| m.free_form)
}

/// Reads and parses one playbook. A file that is not there stops the run with exit 1; a file
/// that is there and does not make sense stops it with exit 4. Both codes are the reference's
/// own, measured against ansible-core 2.19.12: `the playbook: x.yml could not be found` exits 1,
/// while a YAML error, a play that is not a mapping, an unknown keyword, a module that cannot be
/// resolved and `a playbook must be a list of plays` all exit 4, with no `PLAY RECAP`.
///
/// Module names are not resolved here. The reference resolves an action while it loads the
/// play, and this engine still refuses an unresolvable one before the first task, but one step
/// later: [`crate::preflight`] does it, together with the keywords the reference accepts and
/// this release cannot execute. Loading and refusing are separated so a playbook parses and
/// lists exactly as it does in the reference, while the run still stops before it can apply
/// half of itself.
pub fn load(path: &Path) -> anyhow::Result<Playbook> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading playbook {}", path.display()))?;
    parse(&text, &path.display().to_string()).map_err(|err| crate::stats::Refusal::or(4, err))
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
    let mut unsupported = Vec::new();
    for (key, _) in map {
        let key = key.as_str().unwrap_or_default();
        // The reference's own words, measured: an attribute it does not have refuses the load
        // with exit 4, and one it has but this release cannot execute is kept for the
        // pre-flight instead of being refused here.
        match play_keyword(key) {
            None => bail!("'{key}' is not a valid attribute for a Play"),
            Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
            Some(_) => {}
        }
    }
    unsupported.sort_unstable();
    let hosts = match field(yaml, "hosts") {
        Some(Yaml::Value(Scalar::String(s))) => s.to_string(),
        // Every entry, or none: `filter_map` here dropped a non-string entry and ran the play
        // against the rest, which is a host list quietly shorter than the one written. The
        // reference refuses it instead, measured: `Hosts list contains an invalid host value:
        // '3'`, exit 4.
        Some(Yaml::Sequence(items)) => items
            .iter()
            .map(|i| {
                i.as_str().ok_or_else(|| {
                    anyhow!("Hosts list contains an invalid host value: '{}'", shown(i))
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(","),
        None => bail!("a play needs 'hosts'"),
        Some(other) => bail!("'hosts' must be a string or a list, found {other:?}"),
    };
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .unwrap_or(&hosts)
        .to_string();
    let gather_facts = boolean(yaml, "gather_facts")?.unwrap_or(true);
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
    let strategy = match field(yaml, "strategy") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(other) => bail!("'strategy' must be a name, found {other:?}"),
    };
    let (r#become, become_user) = escalation(yaml, "")?;
    Ok(Play {
        name,
        hosts,
        gather_facts,
        vars,
        vars_files,
        tasks,
        r#become,
        become_user,
        strategy,
        unsupported,
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
    let mut unsupported = Vec::new();
    for (key, value) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("task '{label}': keys must be strings"))?;
        // A block's three sections are grammar too, even though the compiler that runs them
        // arrives later: taken for a module name they would be refused as a typo.
        if let Some(section) = BLOCK_SECTIONS.iter().copied().find(|s| *s == key) {
            unsupported.push(section);
            continue;
        }
        match task_keyword(key) {
            Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
            Some(_) => {}
            // Whatever is left is the module, which is how the reference reads a task too: a
            // key it does not know is an action, and a second one is the conflict below rather
            // than an unknown-keyword error.
            None => {
                if let Some((first, _)) = &module {
                    bail!("task '{label}': conflicting action statements: {first}, {key}");
                }
                module = Some((key.to_string(), value));
            }
        }
    }
    // `loop_control` is a mapping, so it has a grammar of its own, and reading two of its
    // sub-keys while dropping the rest reopens the silent skip one level below the keyword:
    // `index_var` or `pause` would be accepted, waved past the pre-flight and ignored while the
    // table still said the keyword runs. The reference refuses a sub-key it does not know at
    // load - measured, exit 4, `'nosuch' is not a valid attribute for a LoopControl` - and the
    // ones it knows and this release cannot honour are parked for the pre-flight like any other
    // keyword.
    let mut loop_var = "item".to_string();
    let mut loop_label = None;
    if let Some(control) = field(yaml, "loop_control") {
        let control = control
            .as_mapping()
            .ok_or_else(|| anyhow!("task '{label}': 'loop_control' must be a mapping"))?;
        for (key, value) in control {
            let key = key
                .as_str()
                .ok_or_else(|| anyhow!("task '{label}': 'loop_control' keys must be strings"))?;
            match loop_control_keyword(key) {
                None => bail!("'{key}' is not a valid attribute for a LoopControl"),
                Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
                // The two this release honours, both plain text: the name a loop item is bound
                // to, and the template a looping task shows instead of the item itself.
                Some(k) => {
                    let text = value.as_str().map(str::to_string).ok_or_else(|| {
                        anyhow!("task '{label}': 'loop_control.{}' must be a name", k.name)
                    })?;
                    if k.name == "loop_var" {
                        loop_var = text;
                    } else {
                        loop_label = Some(text);
                    }
                }
            }
        }
    }
    unsupported.sort_unstable();
    // A task whose only content this release cannot execute has no module to find - `block:`
    // and `local_action:` are the two spellings - so it is loaded empty and refused whole by
    // the pre-flight, instead of being blamed for a module it was never asked to name.
    let (module, value) = match module {
        Some(found) => found,
        None if !unsupported.is_empty() => {
            return Ok(PlayTask {
                name: label,
                module: String::new(),
                unsupported,
                ..PlayTask::empty()
            });
        }
        None => bail!("task '{label}': no module given"),
    };
    // Arguments are read only for a module this release can run. The reference resolves the
    // action before it looks at what was handed to it, so a name it cannot resolve is refused
    // for its name and never for the shape of arguments nobody promised to read; the pre-flight
    // raises that refusal a moment later. Reading them anyway made `nosuchmodule: echo a`
    // complain about `key=value`, which sends the operator to the wrong line.
    let mut args = if is_known(&module) {
        module_args(&module, value).with_context(|| format!("task '{label}'"))?
    } else {
        Map::new()
    };
    if let Some(Yaml::Mapping(extra)) = field(yaml, "args") {
        for (k, v) in extra {
            let k = k
                .as_str()
                .ok_or_else(|| anyhow!("task '{label}': 'args' keys must be strings"))?;
            args.insert(k.to_string(), to_json(v)?);
        }
    }
    let ignore_errors = boolean(yaml, "ignore_errors")?.unwrap_or(false);
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
    let (r#become, become_user) = escalation(yaml, &format!("task '{label}': "))?;
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
        r#become,
        become_user,
        unsupported,
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
        // Every entry counts. Measured on ansible-core 2.19.12: a list holding a non-string
        // refuses the load at exit 4 with `Hosts list contains an invalid host value: '3'`.
        // What would make this red: an entry quietly dropped, which runs the play against a
        // shorter host list than the one written and reports success for it.
        let err = parse("- hosts: [web, 3]\n  tasks: []\n", "x.yml").unwrap_err();
        assert!(
            format!("{err:#}").contains("Hosts list contains an invalid host value: '3'"),
            "{err:#}"
        );
        // Measured on ansible-core 2.19.12: a float prints bare with its decimal kept --
        // `Hosts list contains an invalid host value: '3.5'` -- never the
        // `Value(FloatingPoint(OrderedFloat(3.5)))` Rust Debug blob an operator used to see.
        let err = parse("- hosts: [web, 3.5]\n  tasks: []\n", "x.yml").unwrap_err();
        assert!(
            format!("{err:#}").contains("Hosts list contains an invalid host value: '3.5'"),
            "{err:#}"
        );
        // A sequence or a mapping prints the way ansible-core's own Python repr renders one,
        // measured: `'[1, 2]'`, `'{'a': 1}'` -- not a Rust `Vec`/`LinkedHashMap` Debug dump.
        let err = parse("- hosts: [web, [1, 2]]\n  tasks: []\n", "x.yml").unwrap_err();
        assert!(
            format!("{err:#}").contains("Hosts list contains an invalid host value: '[1, 2]'"),
            "{err:#}"
        );
        let err = parse("- hosts: [web, {a: 1}]\n  tasks: []\n", "x.yml").unwrap_err();
        assert!(
            format!("{err:#}").contains("Hosts list contains an invalid host value: '{'a': 1}'"),
            "{err:#}"
        );
    }

    #[test]
    fn key_value_is_parsed_for_a_module_that_is_not_free_form() {
        let pb = parse(
            "- hosts: all\n  tasks:\n    - set_fact: owner=root state=here\n",
            "x.yml",
        )
        .unwrap();
        let t = &pb.plays[0].tasks[0];
        assert_eq!(t.args["owner"], "root");
        assert_eq!(t.args["state"], "here");
    }

    /// An attribute the reference does not have is refused here, in the reference's own words.
    /// One it has but this release cannot execute is a different event and belongs to the
    /// pre-flight, which is why `until` and `strategy` load without complaint below.
    ///
    /// What would make this red: the loader accepting an invented play attribute, which would
    /// leave a typo in a playbook silently doing nothing.
    #[test]
    fn an_attribute_the_reference_does_not_have_is_refused_at_load() {
        let err = parse(
            "- hosts: all\n  nosuchplaykeyword: 1\n  tasks: []\n",
            "x.yml",
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("'nosuchplaykeyword' is not a valid attribute for a Play"),
            "{text}"
        );
        let pb = parse(
            "- hosts: all\n  strategy: free\n  tasks:\n    - name: Later\n      command: echo hi\n      until: x\n",
            "x.yml",
        )
        .expect("the reference has both, so the loader takes both");
        assert_eq!(pb.plays[0].strategy.as_deref(), Some("free"));
        assert_eq!(pb.plays[0].tasks[0].unsupported, ["until"]);
    }

    #[test]
    fn escalation_keywords_are_read_at_both_levels() {
        let pb = parse(
            "- hosts: all\n  become: yes\n  become_user: deploy\n  become_method: sudo\n  tasks:\n    - command: id\n      become: 'false'\n      become_user: postgres\n",
            "x.yml",
        )
        .unwrap();
        let play = &pb.plays[0];
        assert_eq!(play.r#become, Some(true), "'yes' is a boolean to Ansible");
        assert_eq!(play.become_user.as_deref(), Some("deploy"));
        let t = &play.tasks[0];
        assert_eq!(t.r#become, Some(false), "a quoted spelling still reads");
        assert_eq!(t.become_user.as_deref(), Some("postgres"));
        let bare = parse("- hosts: all\n  tasks:\n    - command: id\n", "x.yml").unwrap();
        assert_eq!(
            (
                bare.plays[0].r#become,
                bare.plays[0].tasks[0].r#become.is_none()
            ),
            (None, true),
            "silence at both levels leaves the decision to the variables and the defaults"
        );
    }

    /// Every escalation program other than `sudo` is refused by its own name, at both levels,
    /// and so are the two keywords that would change how `sudo` is invoked. Escalating through
    /// a different program under the same keyword would run the task under rules nobody wrote.
    #[test]
    fn unsupported_become_methods_and_flags_are_refused_by_name() {
        for method in ["su", "doas", "pbrun", "runas"] {
            let err = parse(
                &format!("- hosts: all\n  become: true\n  become_method: {method}\n  tasks: []\n"),
                "x.yml",
            )
            .unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains(method), "{text}");
            assert!(text.contains("not supported yet"), "{text}");
            let err = parse(
                &format!(
                    "- hosts: all\n  tasks:\n    - name: T\n      command: id\n      become_method: {method}\n"
                ),
                "x.yml",
            )
            .unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains(method) && text.contains('T'), "{text}");
        }
        // The two keywords that change how `sudo` itself is invoked are the reference's own,
        // so they load; the pre-flight is what refuses them, and `preflight`'s own tests say
        // so. Handing `sudo` extra flags or a different binary must never just happen.
        for kw in ["become_flags", "become_exe"] {
            let pb = parse(
                &format!("- hosts: all\n  tasks:\n    - command: id\n      {kw}: x\n"),
                "x.yml",
            )
            .unwrap();
            assert_eq!(pb.plays[0].tasks[0].unsupported, [kw], "{kw}");
        }
    }

    #[test]
    fn a_task_needs_exactly_one_module() {
        let err = parse("- hosts: all\n  tasks:\n    - name: Nothing\n", "x.yml").unwrap_err();
        assert!(format!("{err:#}").contains("no module"));
        // The reference's own words, measured: a second key it does not know is a second
        // action, not an unknown keyword.
        let err = parse(
            "- hosts: all\n  tasks:\n    - command: a\n      shell: b\n",
            "x.yml",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("conflicting action statements: command, shell"),
            "{err:#}"
        );
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
    }

    /// A module name is no longer resolved here: the loader takes whatever the reference takes
    /// and `preflight` decides, one step later, whether this release can run it. What would
    /// make this red: the loader refusing a name again, which would stop `--list-tasks` from
    /// reading a playbook written for a release that has the module.
    #[test]
    fn a_module_name_is_kept_whatever_it_names() {
        for module in ["nosuchmodule", "file", "community.general.command"] {
            let pb = parse(
                &format!("- hosts: all\n  tasks:\n    - name: Later\n      {module}: echo a\n"),
                "x.yml",
            )
            .unwrap_or_else(|e| panic!("{module}: {e:#}"));
            assert_eq!(pb.plays[0].tasks[0].module, module);
        }
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

    /// A keyword this release cannot execute is parked on the task rather than refused, so the
    /// pre-flight can refuse the run with the construct's name in hand. What would make this
    /// red: a keyword dropped on the floor here, which is the one way it could reach an
    /// executor that ignores it.
    #[test]
    fn known_but_unsupported_keywords_are_parked_for_the_preflight() {
        for kw in [
            "until",
            "notify",
            "block",
            "delegate_to",
            "become_flags",
            "no_log",
        ] {
            let pb = parse(
                &format!("- hosts: all\n  tasks:\n    - command: echo hi\n      {kw}: x\n"),
                "x.yml",
            )
            .unwrap_or_else(|e| panic!("{kw}: {e:#}"));
            assert_eq!(pb.plays[0].tasks[0].unsupported, [kw], "{kw}");
        }
    }

    /// `loop_control` is a mapping, so the same rule applies one level down: a sub-key the
    /// reference does not have refuses the load in its own words, and one it has that this
    /// release cannot honour is parked for the pre-flight instead of being dropped.
    ///
    /// Measured on ansible-core 2.19.12: `loop_control: {nosuch: 1}` exits 4 with
    /// `'nosuch' is not a valid attribute for a LoopControl`, and `LoopControl.fattributes`
    /// holds `break_when`, `extended`, `extended_allitems`, `index_var`, `label`, `loop_var`
    /// and `pause`.
    ///
    /// What would make this red: a sub-key read for `loop_var` and `label` alone, with the rest
    /// accepted and silently ignored - a run reporting success having skipped the `index_var`
    /// or the `pause` the operator wrote.
    #[test]
    fn a_loop_control_sub_key_is_honoured_parked_or_refused() {
        let task = |sub: &str| {
            format!(
                "- hosts: all\n  tasks:\n    - debug:\n        msg: x\n      loop: [a]\n      loop_control:\n        {sub}\n"
            )
        };
        let err = parse(&task("nosuch: 1"), "x.yml").unwrap_err();
        assert!(
            format!("{err:#}").contains("'nosuch' is not a valid attribute for a LoopControl"),
            "{err:#}"
        );
        for kw in [
            "break_when",
            "extended",
            "extended_allitems",
            "index_var",
            "pause",
        ] {
            let pb = parse(&task(&format!("{kw}: probe")), "x.yml")
                .unwrap_or_else(|e| panic!("{kw}: {e:#}"));
            assert_eq!(pb.plays[0].tasks[0].unsupported, [kw], "{kw}");
        }
        let pb = parse(&task("loop_var: thing\n        label: shown"), "x.yml").unwrap();
        let t = &pb.plays[0].tasks[0];
        assert_eq!(
            (t.loop_var.as_str(), t.loop_label.as_deref()),
            ("thing", Some("shown"))
        );
        assert!(t.unsupported.is_empty());
    }

    /// A keyword whose value has to be a boolean refuses the load when it is not one, rather
    /// than quietly becoming the default.
    ///
    /// Measured on ansible-core 2.19.12: `gather_facts: maybe` exits 4 with
    /// `Error processing keyword 'gather_facts': The value 'maybe' could not be converted to
    /// 'bool'.`; `ignore_errors: maybe` and `become: maybe` fail the task at run time with the
    /// same sentence. `1` and `0` are booleans there and `2` is not.
    ///
    /// What would make this red: `unwrap_or(false)` back on `ignore_errors` or `unwrap_or(true)`
    /// back on `gather_facts`, which runs the playbook under a value nobody wrote and nobody is
    /// told about.
    #[test]
    fn an_unreadable_boolean_is_refused_in_the_reference_s_words() {
        for (text, kw, value) in [
            (
                "- hosts: all\n  gather_facts: maybe\n  tasks:\n    - command: echo hi\n",
                "gather_facts",
                "'maybe'",
            ),
            (
                "- hosts: all\n  tasks:\n    - command: echo hi\n      ignore_errors: maybe\n",
                "ignore_errors",
                "'maybe'",
            ),
            (
                "- hosts: all\n  tasks:\n    - command: echo hi\n      become: maybe\n",
                "become",
                "'maybe'",
            ),
            // Measured on ansible-core 2.19.12 (`become: 1.5` on a task): a float prints bare
            // with its decimal kept, never the `OrderedFloat` wrapper this engine used to leak
            // into the message.
            (
                "- hosts: all\n  tasks:\n    - command: echo hi\n      become: 1.5\n",
                "become",
                "1.5",
            ),
            // Measured: a sequence and a mapping print the way ansible-core's own Python repr
            // renders one -- `[1, 2]`, `{'a': 1}` -- not a Rust `Vec`/`LinkedHashMap` Debug dump.
            (
                "- hosts: all\n  tasks:\n    - command: echo hi\n      become: [1, 2]\n",
                "become",
                "[1, 2]",
            ),
            (
                "- hosts: all\n  tasks:\n    - command: echo hi\n      become: {a: 1}\n",
                "become",
                "{'a': 1}",
            ),
        ] {
            let err = parse(text, "x.yml").unwrap_err();
            assert!(
                format!("{err:#}").contains(&format!(
                    "Error processing keyword '{kw}': The value {value} could not be converted to 'bool'."
                )),
                "{err:#}"
            );
        }
        let pb = parse(
            "- hosts: all\n  gather_facts: 1\n  tasks:\n    - command: echo hi\n      ignore_errors: 0\n",
            "x.yml",
        )
        .unwrap();
        assert!(pb.plays[0].gather_facts && !pb.plays[0].tasks[0].ignore_errors);
        let err = parse(
            "- hosts: all\n  gather_facts: 2\n  tasks:\n    - command: echo hi\n",
            "x.yml",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("The value 2 could not be converted to 'bool'."),
            "{err:#}"
        );
    }

    /// A block has no module of its own, and blaming it for one it never named would send the
    /// operator looking for a typo. Sorted, so a construct carrying two parked keywords names
    /// the same one whichever order the mapping yielded.
    #[test]
    fn a_construct_with_no_module_of_its_own_loads_empty_and_sorted() {
        let pb = parse(
            "- hosts: all\n  tasks:\n    - name: Grouped\n      tags: t\n      block:\n        - command: echo hi\n",
            "x.yml",
        )
        .unwrap();
        let t = &pb.plays[0].tasks[0];
        assert_eq!((t.name.as_str(), t.module.as_str()), ("Grouped", ""));
        assert_eq!(t.unsupported, ["block", "tags"]);
    }
}
