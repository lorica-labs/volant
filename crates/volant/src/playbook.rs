// SPDX-License-Identifier: GPL-3.0-or-later
//! Playbook loading: plays and tasks, with the keywords this release supports.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};
use volant_protocol::modules::{import_module, is_known, native, short_name};

use crate::keywords::{
    BLOCK_SECTIONS, Support, block_keyword, loop_control_keyword, play_keyword, task_keyword,
};
use crate::roles::{RoleEntry, RoleFrom};
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
    pub tasks: Vec<TaskOrBlock>,
    pub pre_tasks: Vec<TaskOrBlock>,
    pub post_tasks: Vec<TaskOrBlock>,
    pub roles: Vec<RoleEntry>,
    /// The directory of the file this play was written in, which is not always the directory of
    /// the playbook the operator named: `import_playbook` splices another file's plays in place,
    /// and everything a play reads from disk - its `vars_files`, its `group_vars/`, its roles -
    /// is looked for beside the file it came from.
    pub dir: PathBuf,
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

/// One entry of a task list: a task, or a block grouping more of them.
#[derive(Debug, Clone)]
pub enum TaskOrBlock {
    Task(PlayTask),
    Block(Block),
}

/// A block: three task lists and the keywords its tasks inherit.
///
/// `keywords` is a [`PlayTask`] with an empty `module`, because a block carries a subset of the
/// task keywords and reading them twice is how the two would drift apart. Nothing runs it; the
/// compiler merges it into every task underneath.
#[derive(Debug, Clone)]
pub struct Block {
    pub body: Vec<TaskOrBlock>,
    pub rescue: Vec<TaskOrBlock>,
    pub always: Vec<TaskOrBlock>,
    pub keywords: PlayTask,
}

#[derive(Debug, Clone)]
pub struct PlayTask {
    pub name: String,
    pub module: String,
    pub args: Map<String, Value>,
    /// Unset when neither the task nor the block above it said anything. It has to be a
    /// three-state value: measured on ansible-core 2.19.12, a block with `ignore_errors: true`
    /// and a task with `ignore_errors: false` inside it fails the run, so the task's own `false`
    /// has to be told apart from its silence.
    pub ignore_errors: Option<bool>,
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
    /// Whether a failure here leaves the host in the play. Silence means no: the keyword is
    /// three-state only so a block and the task under it can disagree, and by the time the
    /// executor reads a task the compiler has already resolved that.
    pub fn ignores_errors(&self) -> bool {
        self.ignore_errors.unwrap_or(false)
    }

    /// A task with nothing in it, for the two shapes the loader builds without reading a
    /// module: a construct the pre-flight is about to refuse, and a block's inherited
    /// keywords. Neither ever reaches an executor as it stands.
    pub(crate) fn empty() -> Self {
        PlayTask {
            name: String::new(),
            module: String::new(),
            args: Map::new(),
            ignore_errors: None,
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

/// The one name in the module column that is not a module: `meta` asks the engine itself for
/// something, so it is read here and compiled into a step of its own instead of travelling to
/// an agent.
pub const META: &str = "meta";

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

/// Whether this task asks the engine for something instead of naming a module to run.
pub fn is_meta(task: &PlayTask) -> bool {
    short_name(&task.module) == META
}

/// Whether the module's string form is one command line rather than `key=value` pairs.
fn is_free_form(module: &str) -> bool {
    native(module).is_some_and(|m| m.free_form) || import_module(module) == Some(true)
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
    read(path, 0).map_err(|err| crate::stats::Refusal::or(4, err))
}

/// How deep `import_playbook` may nest before the run is refused. A file importing itself would
/// otherwise read on until the process ran out of stack, and a refusal naming the depth says
/// which file to look at.
const IMPORT_DEPTH: usize = 32;

fn read(path: &Path, depth: usize) -> anyhow::Result<Playbook> {
    // Exit 1 for a file that is not there, measured, and not the 4 a file that is there and
    // makes no sense gets. It carries its own code so the blanket [`load`] wraps the parse in
    // cannot take it.
    let text = std::fs::read_to_string(path).map_err(|err| {
        crate::stats::Refusal::at(1, format!("reading playbook {}: {err}", path.display()))
    })?;
    parse_at(&text, &path.display().to_string(), &base_dir(path), depth)
}

/// A playbook's own directory, absolute where the filesystem allows it: `group_vars/`,
/// `host_vars/`, relative `vars_files` entries, the `roles/` directory, lookups and the
/// `playbook_dir` variable all resolve against it. An empty parent means the file was named with
/// no directory at all, so it is the working directory.
pub(crate) fn base_dir(path: &Path) -> PathBuf {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    dir.canonicalize().unwrap_or(dir)
}

pub fn parse(text: &str, source: &str) -> anyhow::Result<Playbook> {
    parse_at(text, source, &base_dir(Path::new(source)), 0)
}

/// One playbook, with every `import_playbook` entry replaced in place by the plays of the file
/// it names.
///
/// In place, not appended: measured on ansible-core 2.19.12, a file importing `sub.yml`, then
/// declaring a play of its own, then importing `sub.yml` again runs the three plays in the order
/// they are written. Each imported play keeps the directory of the file it came from, so its
/// roles and its `vars_files` are looked for beside that file and not beside the playbook the
/// operator typed.
fn parse_at(text: &str, source: &str, dir: &Path, depth: usize) -> anyhow::Result<Playbook> {
    let docs = crate::yaml::load(text, source)?;
    let entries = match docs.first() {
        Some(Yaml::Sequence(items)) => items,
        _ => bail!("{source}: a playbook must be a list of plays"),
    };
    let mut plays = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        match imported_playbook(entry)? {
            Some(name) => {
                if depth >= IMPORT_DEPTH {
                    bail!("{source}: 'import_playbook' nests deeper than {IMPORT_DEPTH} levels");
                }
                let path = Path::new(&name);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    dir.join(path)
                };
                plays.extend(
                    read(&path, depth + 1)
                        .with_context(|| format!("{source}: import_playbook {name}"))?
                        .plays,
                );
            }
            None => plays
                .push(parse_play(entry, dir).with_context(|| format!("{source}: play {}", i + 1))?),
        }
    }
    Ok(Playbook { plays })
}

/// The file an `import_playbook` entry names, rendered.
///
/// The reference renders the value with no variables at all - measured, `{{ 'sub' }}.yml` works
/// and `{{ nosuchvar }}.yml` refuses the load with `Error processing keyword 'import_playbook':
/// 'nosuchvar' is undefined`, exit 4. Nothing has an inventory yet at this point, so a name that
/// depends on a host could not be answered anyway.
fn imported_playbook(entry: &Yaml) -> anyhow::Result<Option<String>> {
    let Some(map) = entry.as_mapping() else {
        return Ok(None);
    };
    let value = map.iter().find_map(|(key, value)| {
        matches!(
            key.as_str(),
            Some("import_playbook" | "ansible.builtin.import_playbook")
        )
        .then_some(value)
    });
    let Some(value) = value else {
        return Ok(None);
    };
    let raw = value
        .as_str()
        .ok_or_else(|| anyhow!("'import_playbook' takes a file name"))?;
    if !crate::template::Templar::is_template(raw) {
        return Ok(Some(raw.to_string()));
    }
    let templar = crate::template::Templar::new(PathBuf::from("."));
    let rendered = templar
        .render(raw, &Map::new())
        .map_err(|err| anyhow!("Error processing keyword 'import_playbook': {}", err.0))?;
    match rendered {
        Value::String(name) => Ok(Some(name)),
        other => bail!("Error processing keyword 'import_playbook': {other} is not a file name"),
    }
}

fn parse_play(yaml: &Yaml, dir: &Path) -> anyhow::Result<Play> {
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
    let tasks = task_list(field(yaml, "tasks"), "tasks")?;
    let pre_tasks = task_list(field(yaml, "pre_tasks"), "pre_tasks")?;
    let post_tasks = task_list(field(yaml, "post_tasks"), "post_tasks")?;
    let roles = match field(yaml, "roles") {
        None | Some(Yaml::Value(Scalar::Null)) => Vec::new(),
        Some(node) => parse_role_entries(node).context("'roles'")?,
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
        pre_tasks,
        post_tasks,
        roles,
        dir: dir.to_path_buf(),
        r#become,
        become_user,
        strategy,
        unsupported,
    })
}

/// A play's `roles:` list, or the `dependencies:` of a `meta/main.yml`. The two are the same
/// grammar, read by the same code so they cannot drift: measured, a dependency carries its own
/// parameters and runs before the role that depends on it.
pub(crate) fn parse_role_entries(node: &Yaml) -> anyhow::Result<Vec<RoleEntry>> {
    let Yaml::Sequence(items) = node else {
        bail!("'roles' must be a list of roles");
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| parse_role_entry(item).with_context(|| format!("role {}", i + 1)))
        .collect()
}

/// One role entry: a bare name, or a mapping naming the role and carrying keywords, the four
/// `*_from` selectors, and parameters.
///
/// The split between a keyword and a parameter is measured, not guessed: a free key on the entry
/// is a role parameter and beats a `set_fact` on the same name, while `vars:` on the entry does
/// not - so `vars:` is read as the block keyword it is and only the leftovers are parameters.
fn parse_role_entry(yaml: &Yaml) -> anyhow::Result<RoleEntry> {
    if let Some(name) = yaml.as_str() {
        return Ok(RoleEntry {
            name: name.to_string(),
            from: RoleFrom::default(),
            params: Map::new(),
            keywords: PlayTask::empty(),
        });
    }
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a role must be a name or a mapping"))?;
    let name = match field(yaml, "role").or_else(|| field(yaml, "name")) {
        Some(Yaml::Value(Scalar::String(s))) => s.to_string(),
        Some(other) => bail!("'role' must be a name, found {other:?}"),
        None => bail!("a role entry needs 'role'"),
    };
    let mut from = RoleFrom::default();
    let mut params = Map::new();
    let mut unsupported = Vec::new();
    for (key, value) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("role '{name}': keys must be strings"))?;
        let selector = match key {
            "tasks_from" => Some(&mut from.tasks),
            "vars_from" => Some(&mut from.vars),
            "defaults_from" => Some(&mut from.defaults),
            "handlers_from" => Some(&mut from.handlers),
            _ => None,
        };
        if let Some(slot) = selector {
            *slot = value
                .as_str()
                .ok_or_else(|| anyhow!("role '{name}': '{key}' must be a file name"))?
                .to_string();
            continue;
        }
        if key == "role" || key == "name" {
            continue;
        }
        // A role entry takes the keywords a block takes, which is the list the reference checks
        // one against; anything else is a parameter for the role itself.
        match block_keyword(key) {
            Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
            Some(_) => {}
            None => {
                params.insert(key.to_string(), to_json(value)?);
            }
        }
    }
    unsupported.sort_unstable();
    let context = format!("role '{name}': ");
    let (r#become, become_user) = escalation(yaml, &context)?;
    Ok(RoleEntry {
        name,
        from,
        params,
        keywords: PlayTask {
            name: String::new(),
            ignore_errors: boolean(yaml, "ignore_errors")?,
            timeout: timeout(yaml, &context)?,
            vars: mapping(yaml, "vars", &context)?,
            when: conditions(yaml, "when", &context)?,
            r#become,
            become_user,
            unsupported,
            ..PlayTask::empty()
        },
    })
}

/// The task list of one file, for a role's `tasks/main.yml` and for `import_tasks`.
///
/// Measured on ansible-core 2.19.12: a file whose document is a mapping rather than a list is
/// refused with `included task files must contain a list of tasks`, exit 4, and one that is not
/// there at all is a different refusal with a different code, raised by the caller that knows
/// which path it asked for.
pub(crate) fn parse_tasks_file(path: &Path) -> anyhow::Result<Vec<TaskOrBlock>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let source = path.display().to_string();
    let docs = crate::yaml::load(&text, &source)?;
    match docs.first() {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Vec::new()),
        Some(node @ Yaml::Sequence(_)) => {
            task_list(Some(node), "tasks").with_context(|| source.clone())
        }
        Some(_) => bail!("{source}: included task files must contain a list of tasks"),
    }
}

/// One list of tasks and blocks: a play's `tasks`, or one of a block's three sections. An
/// absent or null list is empty, which is what the reference does with `block: []` - measured,
/// it runs the playbook and shows nothing for the block.
fn task_list(node: Option<&Yaml>, label: &str) -> anyhow::Result<Vec<TaskOrBlock>> {
    match node {
        Some(Yaml::Sequence(items)) => items
            .iter()
            .enumerate()
            .map(|(i, y)| parse_item(y).with_context(|| format!("{label} item {}", i + 1)))
            .collect(),
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Vec::new()),
        _ => bail!("'{label}' must be a list"),
    }
}

/// A task, or a block. A mapping carrying `block`, `rescue` or `always` is a block: the
/// reference decides it the same way, which is why `rescue` on its own is an error about a
/// missing `block` rather than an unknown module.
fn parse_item(yaml: &Yaml) -> anyhow::Result<TaskOrBlock> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a task must be a mapping"))?;
    let has = |name: &str| map.keys().any(|k| k.as_str() == Some(name));
    if has("block") {
        return Ok(TaskOrBlock::Block(parse_block(yaml)?));
    }
    // Measured on ansible-core 2.19.12, exit 4, one sentence per section:
    // `'rescue' keyword cannot be used without 'block'`.
    for section in BLOCK_SECTIONS.iter().filter(|s| **s != "block") {
        if has(section) {
            bail!("'{section}' keyword cannot be used without 'block'");
        }
    }
    Ok(TaskOrBlock::Task(parse_task(yaml)?))
}

/// A block, with its three sections parsed recursively and its inherited keywords read into a
/// module-less [`PlayTask`].
///
/// Every key is checked against the reference's own Block attribute list, so a key that belongs
/// on a task and not on a block - `loop`, `register`, `until` - is refused in the reference's
/// words instead of being taken for a module or quietly ignored.
fn parse_block(yaml: &Yaml) -> anyhow::Result<Block> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a block must be a mapping"))?;
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .unwrap_or("block")
        .to_string();
    let mut unsupported = Vec::new();
    for (key, _) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("block '{name}': keys must be strings"))?;
        match block_keyword(key) {
            // The reference's own words, measured: a module key next to `block:` reads as an
            // attribute the block does not have, `'debug' is not a valid attribute for a Block`.
            None => bail!("'{key}' is not a valid attribute for a Block"),
            Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
            Some(_) => {}
        }
    }
    unsupported.sort_unstable();
    let context = format!("block '{name}': ");
    let (r#become, become_user) = escalation(yaml, &context)?;
    Ok(Block {
        body: task_list(field(yaml, "block"), "block")?,
        rescue: task_list(field(yaml, "rescue"), "rescue")?,
        always: task_list(field(yaml, "always"), "always")?,
        keywords: PlayTask {
            name,
            ignore_errors: boolean(yaml, "ignore_errors")?,
            timeout: timeout(yaml, &context)?,
            vars: mapping(yaml, "vars", &context)?,
            when: conditions(yaml, "when", &context)?,
            r#become,
            become_user,
            unsupported,
            ..PlayTask::empty()
        },
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
    let mut args = if short_name(&module) == META {
        // `meta` is not a module the agent runs, so its argument is read here and nowhere else:
        // the compiler turns the task into a step of its own and the pre-flight decides whether
        // this release honours the action.
        let action = value
            .as_str()
            .ok_or_else(|| anyhow!("task '{label}': 'meta' takes an action name"))?;
        let mut args = Map::new();
        args.insert("_raw_params".into(), Value::String(action.to_string()));
        args
    } else if is_known(&module) || import_module(&module).is_some() {
        // The three import statements are read here for the same reason `meta` is: the compiler
        // needs what they name, and nothing else ever will, because no step is left for a host.
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
    let context = format!("task '{label}': ");
    let ignore_errors = boolean(yaml, "ignore_errors")?;
    let timeout = timeout(yaml, &context)?;
    let vars = mapping(yaml, "vars", &context)?;
    let when = conditions(yaml, "when", &context)?;
    let changed_when = conditions(yaml, "changed_when", &context)?;
    let failed_when = conditions(yaml, "failed_when", &context)?;
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
    let (r#become, become_user) = escalation(yaml, &context)?;
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

/// `timeout`, on a task or on a block.
fn timeout(yaml: &Yaml, context: &str) -> anyhow::Result<Option<u64>> {
    match field(yaml, "timeout") {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(Yaml::Value(Scalar::Integer(i))) if *i >= 0 => Ok(Some(*i as u64)),
        Some(other) => {
            bail!("{context}'timeout' must be a non-negative integer, found {other:?}")
        }
    }
}

/// A keyword whose value has to be a mapping, kept raw: `vars`, on a task or on a block.
fn mapping(yaml: &Yaml, key: &str, context: &str) -> anyhow::Result<Map<String, Value>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Map::new()),
        Some(v) => match to_json(v).with_context(|| format!("{context}'{key}'"))? {
            Value::Object(map) => Ok(map),
            _ => bail!("{context}'{key}' must be a mapping"),
        },
    }
}

/// `when`, `changed_when`, `failed_when`: one expression or a list of them. A YAML boolean is
/// spelled back as Python would (`True`/`False`) so the expression evaluator reads it.
fn conditions(yaml: &Yaml, key: &str, context: &str) -> anyhow::Result<Vec<String>> {
    let one = |node: &Yaml| -> anyhow::Result<String> {
        match node {
            Yaml::Value(Scalar::String(s)) => Ok(s.to_string()),
            Yaml::Value(Scalar::Boolean(b)) => Ok(if *b { "True" } else { "False" }.to_string()),
            other => bail!(
                "{context}'{key}' must be an expression or a list of expressions, found {other:?}"
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

    /// The tasks of a play, for the tests that wrote none of their own blocks. A block here is
    /// the test's mistake, not the loader's, so it says so rather than being skipped.
    fn tasks(play: &Play) -> Vec<&PlayTask> {
        play.tasks
            .iter()
            .map(|item| match item {
                TaskOrBlock::Task(task) => task,
                TaskOrBlock::Block(_) => panic!("this playbook has no blocks"),
            })
            .collect()
    }

    /// The first task of the first play.
    fn first(pb: &Playbook) -> &PlayTask {
        tasks(&pb.plays[0])[0]
    }

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
        let all = tasks(play);
        assert_eq!(all.len(), 5);

        let t = all[0];
        assert_eq!(
            (t.name.as_str(), t.module.as_str()),
            ("Say hello", "command")
        );
        assert_eq!(t.args["_raw_params"], "echo hello");
        assert!(!t.ignores_errors());

        let t = all[1];
        assert_eq!(t.name, "shell", "unnamed tasks take the module name");
        assert!(t.ignores_errors());

        let t = all[2];
        assert_eq!(t.module, "ansible.builtin.command");
        assert_eq!(t.args["cmd"], "ls -l");
        assert_eq!(t.args["chdir"], "/tmp");

        let t = all[4];
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
        let t = first(&pb);
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
        assert_eq!(first(&pb).unsupported, ["until"]);
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
        let t = tasks(play)[0];
        assert_eq!(t.r#become, Some(false), "a quoted spelling still reads");
        assert_eq!(t.become_user.as_deref(), Some("postgres"));
        let bare = parse("- hosts: all\n  tasks:\n    - command: id\n", "x.yml").unwrap();
        assert_eq!(
            (bare.plays[0].r#become, first(&bare).r#become.is_none()),
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
            assert_eq!(first(&pb).unsupported, [kw], "{kw}");
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
        assert_eq!(first(&pb).args["_raw_params"], "false");
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
        assert_eq!(first(&pb).args["_raw_params"], "echo a=b");
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
            assert_eq!(first(&pb).module, module);
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
        let t = tasks(&pb.plays[0]);
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
        assert_eq!(first(&pb).when, ["False"]);
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
        for kw in ["until", "notify", "delegate_to", "become_flags", "no_log"] {
            let pb = parse(
                &format!("- hosts: all\n  tasks:\n    - command: echo hi\n      {kw}: x\n"),
                "x.yml",
            )
            .unwrap_or_else(|e| panic!("{kw}: {e:#}"));
            assert_eq!(first(&pb).unsupported, [kw], "{kw}");
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
            assert_eq!(first(&pb).unsupported, [kw], "{kw}");
        }
        let pb = parse(&task("loop_var: thing\n        label: shown"), "x.yml").unwrap();
        let t = first(&pb);
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
        assert!(pb.plays[0].gather_facts && !first(&pb).ignores_errors());
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

    /// A construct with no module of its own is loaded empty and refused whole by the
    /// pre-flight, rather than blamed for a module it never named. `local_action` is the shape
    /// that is left now that a block is compiled.
    #[test]
    fn a_construct_with_no_module_of_its_own_loads_empty_and_sorted() {
        let pb = parse(
            "- hosts: all\n  tasks:\n    - name: Grouped\n      tags: t\n      local_action: command echo hi\n",
            "x.yml",
        )
        .unwrap();
        let t = first(&pb);
        assert_eq!((t.name.as_str(), t.module.as_str()), ("Grouped", ""));
        assert_eq!(t.unsupported, ["local_action", "tags"]);
    }

    /// A block takes its three sections and the keywords its tasks inherit, and nests.
    ///
    /// What would make this red: a section read as a module, which is how `rescue:` came to be
    /// refused as an unknown keyword; or a nested block flattened into its parent, which loses
    /// the grouping the operator wrote and with it the recovery attached to it.
    #[test]
    fn a_block_takes_its_sections_and_its_inherited_keywords() {
        let pb = parse(
            "- hosts: all\n  tasks:\n    - name: Grouped\n      block:\n        - command: echo hi\n        - block:\n            - command: echo deeper\n      rescue:\n        - command: echo sorry\n      always:\n        - command: echo done\n      when: ready\n      become: true\n      ignore_errors: true\n      vars: {a: 1}\n",
            "x.yml",
        )
        .unwrap();
        let TaskOrBlock::Block(b) = &pb.plays[0].tasks[0] else {
            panic!("a block");
        };
        assert_eq!(b.body.len(), 2);
        assert!(matches!(b.body[1], TaskOrBlock::Block(_)), "blocks nest");
        assert_eq!(b.rescue.len(), 1);
        assert_eq!(b.always.len(), 1);
        assert_eq!(b.keywords.name, "Grouped");
        assert_eq!(b.keywords.when, ["ready"]);
        assert_eq!(b.keywords.r#become, Some(true));
        assert_eq!(b.keywords.ignore_errors, Some(true));
        assert_eq!(b.keywords.vars["a"], serde_json::json!(1));
        assert!(
            b.keywords.module.is_empty(),
            "a block names no module of its own"
        );
    }

    /// A key the reference's Block attribute list does not have is refused in its own words,
    /// and so is a section written without the block it belongs to.
    ///
    /// Measured on ansible-core 2.19.12, all three at exit 4: `'loop' is not a valid attribute
    /// for a Block`, `'debug' is not a valid attribute for a Block` for a module key sitting
    /// next to `block:`, and `'rescue' keyword cannot be used without 'block'`.
    ///
    /// What would make this red: a block accepting `register` or `loop`, which the reference
    /// refuses - a loop written on a block would then silently run its tasks once.
    #[test]
    fn a_key_a_block_cannot_carry_is_refused_in_the_reference_s_words() {
        for kw in ["loop", "register", "until", "changed_when", "args"] {
            let err = parse(
                &format!(
                    "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      {kw}: x\n"
                ),
                "x.yml",
            )
            .unwrap_err();
            assert!(
                format!("{err:#}")
                    .contains(&format!("'{kw}' is not a valid attribute for a Block")),
                "{kw}: {err:#}"
            );
        }
        let err = parse(
            "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      debug: msg=x\n",
            "x.yml",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("'debug' is not a valid attribute for a Block"),
            "{err:#}"
        );
        for section in ["rescue", "always"] {
            let err = parse(
                &format!("- hosts: all\n  tasks:\n    - {section}:\n        - command: echo hi\n"),
                "x.yml",
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains(&format!(
                    "'{section}' keyword cannot be used without 'block'"
                )),
                "{err:#}"
            );
        }
        // Measured: an empty block is accepted and the playbook runs.
        let pb = parse("- hosts: all\n  tasks:\n    - block: []\n", "x.yml").unwrap();
        let TaskOrBlock::Block(b) = &pb.plays[0].tasks[0] else {
            panic!("a block");
        };
        assert!(b.body.is_empty());
    }

    /// `meta` is read for the action it asks for, whatever that action is: what this release
    /// does about it is the pre-flight's decision, one step later.
    #[test]
    fn a_meta_task_keeps_the_action_it_asked_for() {
        for action in ["noop", "end_play", "nosuchaction"] {
            let pb = parse(
                &format!("- hosts: all\n  tasks:\n    - meta: {action}\n"),
                "x.yml",
            )
            .unwrap_or_else(|e| panic!("{action}: {e:#}"));
            let t = first(&pb);
            assert_eq!(t.module, "meta");
            assert_eq!(t.args["_raw_params"], serde_json::json!(action));
        }
    }
}
