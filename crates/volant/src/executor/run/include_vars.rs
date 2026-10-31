// SPDX-License-Identifier: GPL-3.0-or-later
//! `include_vars`, run where every other controller-side module runs.
//!
//! Measured on ansible-core 2.19.12: a file that is there sets each of its keys as a fact and
//! answers `{"ansible_facts": {...}, "ansible_included_var_files": ["<abs>"], "changed": false}`,
//! a `name:` puts the whole mapping under that one key, and a file that is nowhere fails the task
//! with the paths it looked in spelled out one per line. `dir:` reads every file of a tree, see
//! [`crate::vars::include_dir`].
//!
//! The variables land in their own layer, under `set_fact` and registered values as in the
//! reference, once the task is judged, through [`super::record::record_local_facts`].

use std::path::Path;
use std::sync::Mutex;

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use crate::compile::Step;
use crate::executor::prepare::Item;
use crate::executor::python_type;
use crate::playbook::python_repr;
use crate::vars::{
    Chain, Failure, HostVars, Scan, VarStore, include_dir, include_vars_paths, join_path,
    read_failure, read_vars_document,
};

use super::output::python_falsy;
use super::retry::raised;

/// The key a result names the variables it set that are data under, for
/// [`super::record::record_local_facts`]. Like every `_volant_` key it is taken off before anything
/// shows or registers the result.
pub(super) const DATA_KEY: &str = "_volant_data";

/// The options that only mean something for a directory, and so cannot be given with a file.
const DIR_OPTIONS: [&str; 6] = [
    "dir",
    "depth",
    "files_matching",
    "ignore_files",
    "extensions",
    "ignore_unknown_extensions",
];

/// What the reference's `include_vars` does on a failure it caught: a `message` beside the
/// variables read so far, and the generic sentence a task that did not return a `msg` gets.
struct Returned {
    message: String,
    facts: Map<String, Value>,
    files: Vec<String>,
    encrypted: bool,
    warnings: Vec<&'static str>,
}

impl Returned {
    fn of(message: impl Into<String>) -> Self {
        Returned {
            message: message.into(),
            facts: Map::new(),
            files: Vec::new(),
            encrypted: false,
            warnings: Vec::new(),
        }
    }
}

/// An exception the action raised: the task fails with this text and nothing else.
fn raise(text: &str) -> TaskResult {
    raised(&format!("Task failed: {text}"), Vec::new())
}

/// An exception the action raised from inside another one: the error block prints the text a
/// second time as the cause, and names the file it is about when there is one.
fn chained(text: &str, file: Option<String>) -> TaskResult {
    let at = file.map(|path| crate::playbook::Origin {
        path: path.into(),
        line: 0,
        col: 0,
    });
    raised(
        &format!("Task failed: {text}"),
        vec![crate::render::cause(text, at.as_ref(), None)],
    )
}

/// Python's `str()` of a value, as the reference formats a pattern it was handed.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => python_repr(other),
    }
}

/// The type name a Python comparison or a hash of `value` names in its error: the controller's
/// tagged variants of the builtin types.
fn tagged_type(value: &Value) -> String {
    match value {
        Value::String(_) | Value::Array(_) | Value::Object(_) => {
            format!("_AnsibleTagged{}", {
                let name = python_type(value);
                let mut chars = name.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            })
        }
        other => python_type(other).to_string(),
    }
}

/// `include_vars`, for one item of the task.
pub(super) fn run_include_vars(item: &Item, step: &Step, store: &Mutex<VarStore>) -> TaskResult {
    let args = &item.args;
    let wants_dir = DIR_OPTIONS.iter().any(|k| args.contains_key(*k));
    let wants_file = args.contains_key("file") || args.contains_key("_raw_params");
    if wants_dir && wants_file {
        return raise("You are mixing file only and dir only arguments, these are incompatible");
    }
    let truthy = |key: &str| args.get(key).filter(|v| !python_falsy(v));
    let outcome = match truthy("dir") {
        Some(dir) => read_directory(dir, item, step),
        None => read_file(item, step, store),
    };
    let (facts, files, encrypted) = match outcome {
        Ok(read) => read,
        Err(Done::Raised(result)) => return result,
        Err(Done::Returned(failure)) => {
            let failure = *failure;
            let facts = match wrap(truthy("name"), failure.facts) {
                Ok(facts) => facts,
                Err(result) => return result,
            };
            return failed(Returned { facts, ..failure });
        }
    };
    let facts = match wrap(truthy("name"), facts) {
        Ok(facts) => facts,
        Err(result) => return result,
    };
    let (facts, data) = if args.get("hash_behaviour").and_then(Value::as_str) == Some("merge") {
        merged_over(&item.vars, facts)
    } else {
        (facts, Vec::new())
    };
    let mut r = Map::new();
    r.insert("ansible_facts".into(), Value::Object(facts));
    r.insert("ansible_included_var_files".into(), json!(files));
    r.insert("changed".into(), json!(false));
    r.insert("failed".into(), json!(false));
    if encrypted {
        r.insert("_ansible_no_log".into(), json!(true));
    }
    if !data.is_empty() {
        r.insert(DATA_KEY.into(), json!(data));
    }
    TaskResult(r)
}

/// How a read ended when it did not end in variables.
enum Done {
    /// The action raised.
    Raised(TaskResult),
    /// The module answered a failure.
    Returned(Box<Returned>),
}

type Read = (Map<String, Value>, Vec<String>, bool);

/// The variables under the one name `name:` gave, the reference's `scope[name] = results`, or as
/// they are. A name that is not hashable fails the way Python's dict does.
fn wrap(name: Option<&Value>, facts: Map<String, Value>) -> Result<Map<String, Value>, TaskResult> {
    let Some(name) = name else {
        return Ok(facts);
    };
    let key = match name {
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => {
            return Err(raise(&format!("unhashable type: '{}'", tagged_type(name))));
        }
        other => other.to_string(),
    };
    let mut scope = Map::new();
    scope.insert(key, Value::Object(facts));
    Ok(scope)
}

/// `hash_behaviour: merge`: each name the result sets, merged over the variable of that name the
/// task already sees when both are mappings. Any other `hash_behaviour` replaces, which is what
/// the result already does.
///
/// The second answer names the merged variables that hold what a managed host returned, whichever
/// of their leaves it is: they are data, as a `set_fact` value built from such text is. A value
/// that replaces another takes the status of the new value, which is the file's own text.
fn merged_over(vars: &HostVars, facts: Map<String, Value>) -> (Map<String, Value>, Vec<String>) {
    let mut data = Vec::new();
    let merged = facts
        .into_iter()
        .map(|(key, value)| {
            let merged = match (vars.get(&key), &value) {
                (Some(Value::Object(old)), Value::Object(fresh)) => {
                    if vars.untrusted.contains(&key) {
                        data.push(key.clone());
                    }
                    Value::Object(crate::vars::merge_hash(old, fresh))
                }
                _ => value,
            };
            (key, merged)
        })
        .collect();
    (merged, data)
}

fn failed(failure: Returned) -> TaskResult {
    let mut body = Map::new();
    body.insert("ansible_facts".into(), Value::Object(failure.facts));
    body.insert("ansible_included_var_files".into(), json!(failure.files));
    body.insert("changed".into(), json!(false));
    body.insert("message".into(), json!(failure.message));
    // The sentence of a task that died rather than failed, as the reference words it.
    body.insert(
        "msg".into(),
        json!("Task failed: Action failed: Unknown error."),
    );
    body.insert("failed".into(), json!(true));
    if !failure.warnings.is_empty() {
        body.insert("warnings".into(), json!(failure.warnings));
    }
    if failure.encrypted {
        body.insert("_ansible_no_log".into(), json!(true));
    }
    TaskResult(body)
}

/// One file, named by `file:` or by the free-form text, looked for where the reference looks.
fn read_file(item: &Item, step: &Step, store: &Mutex<VarStore>) -> Result<Read, Done> {
    let args = &item.args;
    let named = args
        .get("file")
        .filter(|v| !python_falsy(v))
        .or_else(|| args.get("_raw_params"))
        .and_then(Value::as_str)
        .map(|text| text.trim_end_matches('\n'))
        .filter(|text| !text.is_empty());
    let Some(name) = named else {
        let mut failure = Returned::of(
            "Could not find file on the Ansible Controller.\nIf you are using a module and expect the file to exist on the remote, see the remote_src option",
        );
        failure.warnings = vec!["Invalid request to find a file that matches a \"null\" value"];
        return Err(Done::Returned(Box::new(failure)));
    };
    let playbook_dir = store
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    let searched = include_vars_paths(
        &step.origin.file_dir,
        step.origin.role_dir.as_deref(),
        &playbook_dir,
        name,
    );
    let Some(path) = searched.iter().find(|p| p.exists()) else {
        // The reference's own wording and its own shape, measured: `ansible_facts` and
        // `ansible_included_var_files` are both present and empty, the paths are listed one per
        // tab-indented line, and `msg` carries the sentence a task that died rather than failed
        // gets.
        return Err(Done::Returned(Box::new(Returned::of(format!(
            "Could not find or access '{name}'\nSearched in:\n{} on the Ansible Controller.\nIf you are using a module and expect the file to exist on the remote, see the remote_src option",
            searched
                .iter()
                .map(|p| format!("\t{}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        )))));
    };
    let shown = path.display().to_string();
    let document = read_vars_document(path)
        .map_err(|err| Done::Returned(Box::new(Returned::of(read_failure(&shown, &err)))))?;
    let facts = match document.value {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        _ => {
            let mut failure = Returned::of(format!("{shown} must be stored as a dictionary/hash"));
            failure.encrypted = document.encrypted;
            return Err(Done::Returned(Box::new(failure)));
        }
    };
    // Each reserved name at its line, the file read again only when it holds one.
    if facts.keys().any(|k| crate::vars::reserved::is_reserved(k)) {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        for name in facts.keys() {
            let at = crate::vars::reserved::top_level_key(&shown, &text, name);
            crate::vars::reserved::warn_if_reserved(name, at.as_ref());
        }
    }
    Ok((facts, vec![shown], document.encrypted))
}

/// Every file of a directory: the options checked in the order the reference checks them, the
/// directory looked for where the reference looks, then the tree read.
fn read_directory(dir: &Value, item: &Item, step: &Step) -> Result<Read, Done> {
    let args = &item.args;
    let fail = |text: &str| Done::Raised(raise(text));
    let extensions = match args.get("extensions") {
        None => ["yaml", "yml", "json"].map(Value::from).to_vec(),
        // The reference turns a string into the list of its characters.
        Some(Value::String(text)) => text.chars().map(|c| Value::from(c.to_string())).collect(),
        Some(Value::Array(list)) => list.clone(),
        Some(_) => {
            return Err(fail(
                "Invalid type for \"extensions\" option, it must be a list",
            ));
        }
    };
    let present = |key: &str| args.get(key).filter(|v| !python_falsy(v));
    let ignore = match present("ignore_files") {
        None => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(text.split_whitespace().map(str::to_string).collect()),
        Some(Value::Object(map)) => Ok(map.keys().cloned().collect()),
        Some(Value::Array(list)) => Ok(list.iter().map(py_str).collect()),
        Some(other) => Err(format!("'{}' object is not iterable", python_type(other))),
    };
    let pattern = present("files_matching").map(py_str);
    // The depth is first compared with a level once the directory is being read.
    let depth = match present("depth") {
        None => Ok(0.0),
        Some(Value::Bool(_)) => Ok(1.0),
        Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(0.0)),
        Some(other) => Err(other.clone()),
    };
    let unknown_ok = present("ignore_unknown_extensions").is_some();
    let Some(dir) = dir.as_str() else {
        return Err(fail("the 'dir' option must be a string"));
    };
    let top = match step.origin.role_dir.as_deref() {
        Some(role) => role_directory(role, dir),
        None => join_path(&step.origin.file_dir.to_string_lossy(), dir),
    };
    let scan = Scan::new(pattern.as_deref(), ignore, extensions, unknown_ok)
        .map_err(|text| fail(&text))?;
    let path = Path::new(&top);
    if !path.exists() {
        return Err(Done::Returned(Box::new(Returned::of(format!(
            "{top} directory does not exist"
        )))));
    }
    if !path.is_dir() {
        return Err(Done::Returned(Box::new(Returned::of(format!(
            "{top} is not a directory"
        )))));
    }
    let depth = depth.map_err(|bad| {
        fail(&format!(
            "'>' not supported between instances of 'int' and '{}'",
            tagged_type(&bad)
        ))
    })?;
    match include_dir(&top, &scan.with_depth(depth)) {
        Ok(read) => Ok((read.facts, read.files, read.encrypted)),
        Err(Failure::Returned {
            message,
            facts,
            files,
            encrypted,
        }) => Err(Done::Returned(Box::new(Returned {
            message,
            facts,
            files,
            encrypted,
            warnings: Vec::new(),
        }))),
        Err(Failure::Raised { text, chain }) => Err(Done::Raised(match chain {
            Chain::Alone => raise(&text),
            Chain::Cause => chained(&text, None),
            Chain::CauseAt(path) => chained(&text, Some(path)),
        })),
    }
}

/// Where a task of a role looks for `dir`: a name that starts with `vars/` is the role's own path
/// when that exists, any other is under the role's `vars/`.
fn role_directory(role: &Path, dir: &str) -> String {
    let role = role.to_string_lossy();
    if dir.split('/').next() == Some("vars") {
        let inside = join_path(&role, dir);
        if Path::new(&inside).exists() {
            return inside;
        }
        return dir.to_string();
    }
    join_path(&join_path(&role, "vars"), dir)
}
