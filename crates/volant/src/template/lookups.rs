// SPDX-License-Identifier: GPL-3.0-or-later
//! `lookup(...)`: the lookup plugins the engine has, as one global function.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use minijinja::value::{Kwargs, Rest, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State, Value};

use crate::inventory::Inventory;

use super::{Context, FileRender, Templar, TemplateError, Vars};
use crate::bridge::{Bridge, PluginKind};

pub fn register(env: &mut Environment<'static>, base_dir: PathBuf, bridge: Option<&Arc<Bridge>>) {
    let bridge = bridge.cloned();
    let call = move |state: &State, name: String, terms: Rest<Value>, kwargs: Kwargs| {
        // Measured on ansible-core 2.19.12: an undefined term or option is an undefined
        // read (`lookup('env', nope | lower)`, `lookup('vars', 'x', default=nope | lower)`).
        let undefined_option = kwargs
            .args()
            .any(|k| kwargs.peek::<Value>(k).is_ok_and(|v| v.is_undefined()));
        if undefined_option || terms.iter().any(super::holds_undefined) {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        // `ansible.legacy.env` is the engine's `env`, as the reference resolves it.
        let name = match name.strip_prefix("ansible.legacy.") {
            Some(short) if supported(short) => short.to_string(),
            _ => name,
        };
        if let Some(bridge) = &bridge
            && super::bridged_name(PluginKind::Lookup, &name)
        {
            // The plugin shapes its own answer: `wantlist` and `errors` go with the call.
            return super::bridged::lookup(state, bridge, &name, &terms, &kwargs);
        }
        let wantlist = kwargs.get::<Option<bool>>("wantlist")?.unwrap_or(false);
        // As `_invoke_lookup` does: `warn` and `ignore` turn a failed lookup into `None` (`[]`
        // with `wantlist`); any other value, `strict` included, raises.
        let errors = kwargs.get::<Option<String>>("errors")?;
        let results = match lookup(state, &name, &terms, kwargs, &base_dir, bridge.as_ref()) {
            Ok(results) => results,
            Err(err) if matches!(errors.as_deref(), Some("warn" | "ignore")) => {
                if errors.as_deref() == Some("warn") {
                    eprintln!(
                        "[WARNING]: An error occurred while running the lookup plugin '{name}'. \
                         Error was {}",
                        err.detail().unwrap_or_default()
                    );
                }
                return Ok(if wantlist {
                    Value::from(Vec::<Value>::new())
                } else {
                    Value::from(())
                });
            }
            Err(err) => return Err(err),
        };
        Ok(if wantlist {
            Value::from(results)
        } else {
            shaped(results)
        })
    };
    // `query(name, ...)` and its alias `q`, routed here (`route_calls`), are `lookup(name, ...,
    // wantlist=True)`, a `wantlist` the call gives replaced, as ansible-core's `_query` does.
    let lookup_call = call.clone();
    let query = move |state: &State, name: String, terms: Rest<Value>, kwargs: Kwargs| {
        let kwargs: Kwargs = kwargs
            .args()
            .filter(|k| *k != "wantlist")
            .map(|k| (k.to_string(), kwargs.peek::<Value>(k).unwrap_or_default()))
            .chain(std::iter::once(("wantlist".to_string(), Value::from(true))))
            .collect();
        lookup_call(state, name, terms, kwargs)
    };
    env.add_function("lookup", call);
    env.add_function(super::QUERY_KEY, query);
}

/// Marks the render as data, see [`super::TaintSink`].
pub(super) fn taint(state: &State) {
    if let Some(sink) = state.lookup(super::TAINT_KEY)
        && let Some(sink) = sink.downcast_object_ref::<super::TaintSink>()
    {
        sink.taint();
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, msg.into())
}

/// The lookups this release runs, short names; each is also accepted as `ansible.builtin.<name>`.
const SUPPORTED: &[&str] = &[
    "env",
    "file",
    "fileglob",
    "first_found",
    "inventory_hostnames",
    "pipe",
    "template",
    "unvault",
    "vars",
    "varnames",
];

/// Whether `lookup(name)` runs in this release. A pure name check: it never runs the lookup.
pub fn supported(name: &str) -> bool {
    SUPPORTED.contains(&name.strip_prefix("ansible.builtin.").unwrap_or(name))
}

/// `lookup('env'|'file'|'vars'|'pipe'|'template', term...)`: the plugin's own list, one result
/// per term. `first_found` reads all its terms as one search.
#[expect(clippy::too_many_lines)]
fn lookup(
    state: &State,
    name: &str,
    terms: &[Value],
    kwargs: Kwargs,
    base_dir: &Path,
    bridge: Option<&Arc<Bridge>>,
) -> Result<Vec<Value>, Error> {
    if !supported(name) {
        return Err(invalid(format!(
            "lookup plugin ({name}) is not available yet"
        )));
    }
    if matches!(name, "first_found" | "ansible.builtin.first_found") {
        return first_found(state, terms, kwargs, base_dir);
    }
    if matches!(name, "fileglob" | "ansible.builtin.fileglob") {
        return fileglob(state, terms, kwargs, base_dir);
    }
    if matches!(
        name,
        "inventory_hostnames" | "ansible.builtin.inventory_hostnames"
    ) {
        kwargs.assert_all_used()?;
        return inventory_hostnames(state, terms);
    }
    if matches!(name, "varnames" | "ansible.builtin.varnames") {
        kwargs.assert_all_used()?;
        return varnames(state, terms);
    }
    let default_value: Option<Value> = kwargs.get::<Option<Value>>("default")?;
    kwargs.assert_all_used()?;
    // Whatever a lookup read at run time is data: a file's text, a command's output, an
    // environment variable. Measured on ansible-core 2.19.12, all three come back carrying the
    // tag that stops the engine templating them again, and the three display the same
    // `{{ 1 + 1 }}` the file that held it did. `vars` is not one of them: it hands back a value
    // the context already holds, and reading an untrusted name through it taints on the same
    // path a bare read does.
    let taint = || taint(state);
    let mut results = Vec::new();
    for term in terms {
        let term_text = term
            .as_str()
            .map_or_else(|| term.to_string(), str::to_string);
        let found = match name {
            "env" | "ansible.builtin.env" => {
                taint();
                Value::from(std::env::var(&term_text).unwrap_or_default())
            }
            "file" | "ansible.builtin.file" => {
                let path = found_file(state, &term_text, base_dir)?;
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    invalid(format!(
                        "could not locate file in lookup: {}: {e}",
                        path.display()
                    ))
                })?;
                taint();
                Value::from(text.trim_end_matches(['\r', '\n']))
            }
            // The file as it reads once decrypted, a plain one as it is, and no newline dropped:
            // measured, `lookup('unvault', 'f.txt')` of `txt_secret\n` keeps the newline.
            "unvault" | "ansible.builtin.unvault" => {
                let path = found_file(state, &term_text, base_dir)?;
                let bytes = std::fs::read(&path).map_err(|e| {
                    invalid(format!(
                        "could not locate file in lookup: {}: {e}",
                        path.display()
                    ))
                })?;
                let bytes = if crate::vault::is_encrypted(&bytes) {
                    crate::vault::decrypt(&bytes, &crate::vault::run_secrets())
                        .map_err(|e| invalid(e.0))?
                } else {
                    bytes
                };
                taint();
                Value::from(String::from_utf8_lossy(&bytes).into_owned())
            }
            "vars" | "ansible.builtin.vars" => match state.lookup(&term_text) {
                Some(v) if !v.is_undefined() => v,
                _ => match &default_value {
                    Some(d) => d.clone(),
                    None => {
                        return Err(invalid(format!(
                            "No variable found with this name: {term_text}"
                        )));
                    }
                },
            },
            "pipe" | "ansible.builtin.pipe" => {
                taint();
                let out = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&term_text)
                    .output()
                    .map_err(|e| invalid(format!("lookup pipe: {e}")))?;
                if !out.status.success() {
                    return Err(invalid(format!(
                        "lookup_plugin.pipe({term_text}) returned {}",
                        out.status.code().unwrap_or(-1)
                    )));
                }
                Value::from(
                    String::from_utf8_lossy(&out.stdout)
                        .trim_end_matches(['\r', '\n'])
                        .to_string(),
                )
            }
            // A template's text is author content read on the controller, but what it renders to
            // is data: measured on ansible-core 2.19.12, `templates/t.j2` holding
            // `{{ '{{ 1 + 1 }}' }}` shows `{{ 1 + 1 }}` through `lookup('template', 't.j2')`,
            // never `2`, exactly as `lookup('file')` does.
            "template" | "ansible.builtin.template" => {
                let path = find_template(state, &term_text, base_dir)?;
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| invalid(format!("template lookup: {}: {e}", path.display())))?;
                taint();
                render_template(state, &term_text, &path, &text, base_dir, bridge)?
            }
            _ => unreachable!("supported() admitted {name}"),
        };
        results.push(found);
    }
    Ok(results)
}

/// `find_file_in_search_path`: each entry's `files/<term>`, then its `<term>`.
fn found_file(state: &State, term: &str, base_dir: &Path) -> Result<PathBuf, Error> {
    let search = search_path(state, base_dir);
    relative_stack(&search, Some("files"), term)
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| invalid(format!("could not locate file in lookup: {term}")))
}

/// `lookup('inventory_hostnames', pattern...)`: the hosts of the inventory the pattern selects,
/// matched with the pattern grammar `hosts:` and `--limit` use, over the `groups` the run has.
fn inventory_hostnames(state: &State, terms: &[Value]) -> Result<Vec<Value>, Error> {
    let mut groups = BTreeMap::new();
    if let Some(all) = state.lookup("groups")
        && let Ok(names) = all.try_iter()
    {
        for name in names {
            let hosts = all.get_item(&name).ok().and_then(|h| h.try_iter().ok());
            let hosts = hosts.into_iter().flatten().map(|h| text_of(&h)).collect();
            groups.insert(text_of(&name), hosts);
        }
    }
    let pattern = terms.iter().map(text_of).collect::<Vec<_>>().join(":");
    let resolution = Inventory::from_groups(&groups).resolve(&pattern);
    // Said once per run, as the reference's display drops a warning it has already shown.
    static SHOWN: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    for unmatched in &resolution.unmatched {
        let line =
            format!("[WARNING]: Could not match supplied host pattern, ignoring: {unmatched}");
        if SHOWN.lock().expect("warnings shown").insert(line.clone()) {
            eprintln!("{line}");
        }
    }
    Ok(resolution
        .hosts
        .into_iter()
        .map(|h| Value::from(h.name))
        .collect())
}

/// `lookup('varnames', regex...)`: the names of the variables in scope that each regex finds
/// somewhere in them, term by term, as Python's `re.search` does.
fn varnames(state: &State, terms: &[Value]) -> Result<Vec<Value>, Error> {
    let names: Vec<String> = state
        .lookup(super::CONTEXT_KEY)
        .and_then(|root| root.try_iter().ok())
        .into_iter()
        .flatten()
        .map(|n| text_of(&n))
        .filter(|n| !n.starts_with("__volant_") && n != "volant_version")
        .collect();
    let mut found = Vec::new();
    for term in terms {
        let Some(pattern) = term.as_str() else {
            return Err(invalid(format!(
                "Invalid setting identifier, \"{term}\" is not a string, its a {}",
                python_type(term)
            )));
        };
        let regex = regex::Regex::new(pattern).map_err(|e| {
            invalid(format!(
                "Unable to use \"{pattern}\" as a search parameter: {e}"
            ))
        })?;
        found.extend(
            names
                .iter()
                .filter(|n| regex.is_match(n))
                .map(|n| Value::from(n.as_str())),
        );
    }
    Ok(found)
}

/// Python's name for the type of a template value, as an error message prints it.
fn python_type(value: &Value) -> &'static str {
    match value.kind() {
        ValueKind::Bool => "<class 'bool'>",
        ValueKind::Number if value.is_integer() => "<class 'int'>",
        ValueKind::Number => "<class 'float'>",
        ValueKind::Seq => "<class 'list'>",
        ValueKind::Map => "<class 'dict'>",
        ValueKind::None => "<class 'NoneType'>",
        _ => "<class 'str'>",
    }
}

/// The shape a plugin's list takes through `lookup()` without `wantlist=True`, read from
/// ansible-core 2.19.12's `_invoke_lookup` and measured there: no result stays an empty list,
/// one result is that value with its own type (`lookup('fileglob', 'files/a.txt')` answers
/// `type_debug` -> `str`), several are joined with `,` when every one is a string
/// (`lookup('env', 'HOME', 'USER')` -> `/home/user,user`) and stay a list otherwise.
fn shaped(mut results: Vec<Value>) -> Value {
    match results.len() {
        1 => results.remove(0),
        n if n > 1 && results.iter().all(|v| v.kind() == ValueKind::String) => Value::from(
            results
                .iter()
                .map(|v| v.as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(","),
        ),
        _ => Value::from(results),
    }
}

/// `ansible_search_path`, the controller's list of the directories a role's or a playbook's
/// relative names are looked for in. Without it, the directory the lookups are anchored to.
fn search_path(state: &State, base_dir: &Path) -> Vec<PathBuf> {
    match state.lookup("ansible_search_path") {
        Some(v) if v.kind() == ValueKind::Seq => v
            .try_iter()
            .map(|entries| entries.map(|e| base_dir.join(text_of(&e))).collect())
            .unwrap_or_default(),
        _ => vec![base_dir.to_path_buf()],
    }
}

fn text_of(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_string)
}

/// `<entry>/templates/<name>`, then `<entry>/<name>`, for each entry of the search path.
fn find_template(state: &State, name: &str, base_dir: &Path) -> Result<PathBuf, Error> {
    search_path(state, base_dir)
        .into_iter()
        .flat_map(|dir| [dir.join("templates").join(name), dir.join(name)])
        .find(|p| p.exists())
        .ok_or_else(|| {
            invalid(format!(
                "the template file {name} could not be found for the lookup"
            ))
        })
}

thread_local! {
    /// How many `lookup('template')` renders are open on this thread, one inside the other.
    static TEMPLATE_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Deeper than this, a template that looks itself up would overflow the stack rather than fail.
const MAX_TEMPLATE_DEPTH: usize = 32;

/// The file rendered once, with the `template` module's default options, against the variables
/// and the trust of the render that called the lookup. A lookup reaches only the borrowed
/// `Environment` behind its `State`, not a `Templar`, so it builds one: a render per lookup call
/// does not make keeping one worth it.
///
/// Two things the reference's variables have that the calling context's map may not:
///
/// 1. A name the template reads is resolved first when it still holds a template of its own.
///    The reference templates a variable lazily, when it is read; here the lookup can run inside
///    `resolve_vars_tainted`'s pass, whose map still holds `a: "{{ b }}"`, and a single-pass
///    render would write `{{ b }}` into the result, which is then data and never rendered again.
///    A name from a managed host is left as it is: it is data, and rendering it is the hole.
/// 2. `plugins/lookup/template.py` adds `generate_ansible_template_vars(path=term,
///    fullpath=lookupfile, include_ansible_managed='ansible_managed' not in vars)`:
///    `template_path`, `template_fullpath`, and `ansible_managed` only when it is absent, as the
///    `template` action does. They are the controller's own words, so they are trusted.
fn render_template(
    state: &State,
    name: &str,
    path: &Path,
    text: &str,
    base_dir: &Path,
    bridge: Option<&Arc<Bridge>>,
) -> Result<Value, Error> {
    let depth = TEMPLATE_DEPTH.get();
    if depth >= MAX_TEMPLATE_DEPTH {
        return Err(invalid(format!(
            "template lookup: maximum recursion depth exceeded rendering {name}"
        )));
    }
    TEMPLATE_DEPTH.set(depth + 1);
    let out = render_template_at(state, name, path, text, base_dir, bridge);
    TEMPLATE_DEPTH.set(depth);
    out.map(Value::from).map_err(|e| lookup_error(name, &e))
}

fn render_template_at(
    state: &State,
    name: &str,
    path: &Path,
    text: &str,
    base_dir: &Path,
    bridge: Option<&Arc<Bridge>>,
) -> Result<String, TemplateError> {
    let templar = Templar::with_bridge(base_dir.to_path_buf(), bridge.cloned());
    let root = state.lookup(super::CONTEXT_KEY);
    let ctx = root
        .as_ref()
        .and_then(|r| r.downcast_object_ref::<Context>());
    let mut map = ctx.map(|c| c.vars.clone()).unwrap_or_default();
    let mut untrusted: BTreeSet<String> = ctx.map(|c| c.untrusted.clone()).unwrap_or_default();
    let hostvars = ctx.and_then(|c| c.hostvars.as_ref());
    let shared = ctx.and_then(|c| c.shared.as_ref());
    let facts = ctx.and_then(|c| c.facts.as_ref());
    let untrusted_hosts = ctx.map(|c| &c.untrusted_hosts);

    let read = templar
        .env()
        .template_from_str(text)
        .map(|t| t.undeclared_variables(false))
        .unwrap_or_default();
    for var in read {
        if untrusted.contains(&var) || !map.get(&var).is_some_and(super::holds_template) {
            continue;
        }
        let rendered = templar.render_value_tainted(
            &map[&var],
            Vars {
                map: &map,
                hostvars,
                shared,
                facts,
                untrusted: Some(&untrusted),
                untrusted_hosts,
            },
        );
        // Lenient, like `resolve_vars_tainted`: the name may sit in a branch that never runs, or
        // behind `default`. Dropped, it reads as undefined, so a read that does happen fails
        // there, lazily, the way the reference's does.
        let Ok((resolved, from_host)) = rendered else {
            map.remove(&var);
            continue;
        };
        if from_host {
            untrusted.insert(var.clone());
        }
        map.insert(var, resolved);
    }

    if !map.contains_key("ansible_managed") {
        map.insert("ansible_managed".into(), "Ansible managed".into());
    }
    map.insert("template_path".into(), name.into());
    map.insert(
        "template_fullpath".into(),
        path.display().to_string().into(),
    );
    untrusted.remove("template_path");
    untrusted.remove("template_fullpath");
    // The two names just written beat a gathered fact of the same name, which would otherwise
    // answer first from its own layer.
    let named = ["template_path", "template_fullpath"];
    let facts = facts.map(|f| {
        if named.iter().any(|k| f.contains_key(*k)) {
            let mut f = serde_json::Map::clone(f);
            named.iter().for_each(|k| {
                f.shift_remove(*k);
            });
            Arc::new(f)
        } else {
            Arc::clone(f)
        }
    });
    let vars = Vars {
        map: &map,
        hostvars,
        shared,
        facts: facts.as_ref(),
        untrusted: Some(&untrusted),
        untrusted_hosts,
    };
    templar.render_file(text, vars, &FileRender::default())
}

fn lookup_error(name: &str, e: &TemplateError) -> Error {
    if e.is_undefined() {
        // Kept an undefined error, so the caller's own render reports it as one.
        let detail =
            e.0.strip_prefix(super::UNDEFINED)
                .unwrap_or(&e.0)
                .trim_start();
        Error::new(
            ErrorKind::UndefinedError,
            format!("in the template {name}: {detail}"),
        )
    } else {
        invalid(format!("in the template {name}: {e}"))
    }
}

/// `lookup('fileglob', pattern...)`, `plugins/lookup/fileglob.py`, read on the dev machine
/// (ansible-core 2.19.12): a pattern with no directory of its own is globbed under `files/` of
/// each entry of `ansible_search_path`, then under the entry itself, the first entry with a
/// match winning; a pattern that already names a directory - an absolute one, measured on the
/// two static `with_fileglob` patterns of the `airgap` role, or a relative one under a search
/// entry - is globbed at that directory directly. Only files match, in the order the directory
/// lists them (`glob.glob`, no sort, no dedupe), and several terms concatenate. The result then
/// goes through the same shaping every other plugin's does ([`shaped`]).
///
/// The result is controller content, not tainted: a matched name is a path the pattern's author
/// named and the controller's filesystem confirmed, the same standing `lookup('first_found')`
/// gives the path it finds. Nothing here reads a managed host.
fn fileglob(
    state: &State,
    terms: &[Value],
    kwargs: Kwargs,
    base_dir: &Path,
) -> Result<Vec<Value>, Error> {
    kwargs.assert_all_used()?;
    let search = search_path(state, base_dir);
    let mut out: Vec<String> = Vec::new();
    for term in terms {
        let pattern = text_of(term);
        for dir in fileglob_dirs(&pattern, &search) {
            let matched = glob_files(&dir, fileglob_file(&pattern))
                .into_iter()
                .map(found_path)
                .collect::<Result<Vec<_>, _>>()?;
            if !matched.is_empty() {
                out.extend(matched);
                break;
            }
        }
    }
    Ok(out.into_iter().map(Value::from).collect())
}

/// The glob part of a pattern: everything after its last `/`, or the whole pattern when it names
/// no directory.
fn fileglob_file(pattern: &str) -> &str {
    pattern.rsplit('/').next().unwrap_or(pattern)
}

/// The directories a pattern is globbed in, in the order they are tried. A pattern with no `/`
/// is the reference's bare-name case: `files/` of each search entry, then the entry itself. A
/// pattern that names its own directory is globbed there directly - the entry itself when that
/// directory is absolute (the `airgap` role's own patterns, `{{ airgap_dir }}/*.tar.gz` rendered
/// to an absolute path), each search entry's own copy of it otherwise.
fn fileglob_dirs(pattern: &str, search: &[PathBuf]) -> Vec<PathBuf> {
    match pattern.rsplit_once('/') {
        None => search
            .iter()
            .flat_map(|dir| [dir.join("files"), dir.clone()])
            .collect(),
        Some(("", _)) => vec![PathBuf::from("/")],
        Some((dir, _)) if Path::new(dir).is_absolute() => vec![PathBuf::from(dir)],
        Some((dir, _)) => search
            .iter()
            .flat_map(|entry| [entry.join("files").join(dir), entry.join(dir)])
            .collect(),
    }
}

/// One directory read non-recursively for the files (not the directories) whose name matches a
/// shell glob pattern (`*`, `?`, `[...]`, `[!...]`), Python's `fnmatch` set. A directory that
/// does not exist, or a pattern that fails to compile, matches nothing rather than erroring: the
/// reference's own `glob.glob` answers the same way for a directory that is not there. A name
/// starting with `.` matches only a pattern starting with `.`, as `glob.glob` without
/// `include_hidden` does.
fn glob_files(dir: &Path, pattern: &str) -> Vec<String> {
    let Ok(re) = regex::Regex::new(&fnmatch_to_regex(pattern)) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let include_hidden = pattern.starts_with('.');
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| (include_hidden || !name.starts_with('.')) && re.is_match(name))
        })
        .map(|entry| entry.path().display().to_string())
        .collect()
}

/// A shell glob pattern (`*`, `?`, a `[...]` or negated `[!...]` character class, everything else
/// literal) as an anchored regular expression.
fn fnmatch_to_regex(pattern: &str) -> String {
    let mut re = String::from("(?s)^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => {
                re.push('[');
                if chars.peek() == Some(&'!') {
                    re.push('^');
                    chars.next();
                }
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                    if "\\^]".contains(c) {
                        re.push('\\');
                    }
                    re.push(c);
                }
                re.push(']');
            }
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    re
}

/// `lookup('first_found', ...)`, `plugins/lookup/first_found.py`: every term, a name, a list of
/// names or a `{files, paths}` mapping, becomes a list of candidates, and each candidate is
/// tried against each entry of `ansible_search_path` as it is written. Measured on ansible-core
/// 2.19.12 in a role holding `files/second.txt`: `second.txt` is not found and
/// `files/second.txt` is, so nothing adds `files/` or `templates/` of its own accord.
fn first_found(
    state: &State,
    terms: &[Value],
    kwargs: Kwargs,
    base_dir: &Path,
) -> Result<Vec<Value>, Error> {
    let mut options = Options {
        files: kwargs.get::<Option<Value>>("files")?.unwrap_or_default(),
        paths: kwargs.get::<Option<Value>>("paths")?.unwrap_or_default(),
        skip: kwargs.get::<Option<bool>>("skip")?.unwrap_or(false),
    };
    kwargs.assert_all_used()?;
    let mut candidates = Vec::new();
    if terms.is_empty() {
        let files = split_on(&options.files, &[',', ';']);
        push_candidates(files, &options.paths, &mut candidates);
    }
    for term in terms {
        candidates_of(term, &mut candidates, &mut options)?;
    }
    let search = search_path(state, base_dir);
    let subdir = state
        .lookup(WITH_SUBDIR)
        .and_then(|v| v.as_str().map(str::to_string));
    for name in &candidates {
        if let Some(found) = relative_stack(&search, subdir.as_deref(), name)
            .into_iter()
            .find(|p| p.exists())
        {
            return Ok(vec![Value::from(found_path(found.display().to_string())?)]);
        }
    }
    if options.skip {
        return Ok(Vec::new());
    }
    Err(invalid("No file was found when using first_found."))
}

/// The directory `with_first_found` searches in each entry before the entry itself, which the
/// task executor sets when it runs the lookup for a `with_first_found` loop and nothing else
/// does: a plain `lookup('first_found')` searches no subdirectory. Two colons cannot appear in a
/// Jinja identifier, so no template can read or write it. `executor/prepare.rs` writes the same
/// literal.
const WITH_SUBDIR: &str = "volant::first_found_subdir";

/// `DataLoader.path_dwim_relative_stack` for one name, as ansible-core 2.19.12's `first_found`
/// calls it (`first_found.py`, `find_file_in_search_path(variables, subdir, fn)`): each search
/// entry's `<subdir>/<name>`, unless the name already starts with that directory, before the
/// entry's own `<name>`. An absolute name is itself whatever it is joined to.
fn relative_stack(search: &[PathBuf], subdir: Option<&str>, name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in search {
        if let Some(sub) = subdir
            && name.split('/').next() != Some(sub)
        {
            out.push(dir.join(sub).join(name));
        }
        out.push(dir.join(name));
    }
    out
}

/// A path `fileglob` or `first_found` found on the controller, refused when its name holds a
/// template marker. The path is bound as the playbook's own content when its terms were, and an
/// author's string is rendered again while it still looks like a template: a file named
/// `{{ lookup('pipe', ...) }}.tar.gz` under a globbed directory would run its command here. The
/// reference never templates a lookup's result again and copies such a file as it is; this
/// release fails the task naming it instead, because binding the path as data would make the
/// plugin that reads it refuse it as a path a host chose.
fn found_path(path: String) -> Result<String, Error> {
    if ["{{", "{%", "{#"]
        .iter()
        .any(|marker| path.contains(marker))
    {
        return Err(invalid(format!(
            "the lookup found '{path}', whose name holds a template marker; rename the file"
        )));
    }
    Ok(path)
}

/// The plugin's options as `first_found.py` holds them: set from the keywords, and replaced
/// whole by `set_options(direct=term)` for each mapping term, so a mapping without `skip` resets
/// it to false and its `paths` are the ones later string terms use.
struct Options {
    files: Value,
    paths: Value,
    skip: bool,
}

/// `_process_terms`: a string is a list of names split on `,` and `;`, joined to the options'
/// `paths`; a mapping sets the options and its `files` joined to its `paths`; a list is each of
/// its terms.
fn candidates_of(term: &Value, out: &mut Vec<String>, options: &mut Options) -> Result<(), Error> {
    match term.kind() {
        ValueKind::String => push_candidates(split_on(term, &[',', ';']), &options.paths, out),
        ValueKind::Seq => {
            for t in term.try_iter()? {
                candidates_of(&t, out, options)?;
            }
        }
        ValueKind::Map => {
            *options = Options {
                files: term.get_attr("files")?,
                paths: term.get_attr("paths")?,
                skip: term.get_attr("skip")?.is_true(),
            };
            let files = split_on(&options.files, &[',', ';']);
            push_candidates(files, &options.paths, out);
        }
        _ => {
            return Err(invalid(format!(
                "Invalid term supplied. A string, dict or list is required, not '{}'.",
                super::type_name(&serde_json::to_value(term).unwrap_or_default())
            )));
        }
    }
    Ok(())
}

/// Each file under each path (split on `,`, `:` and `;`), or the files alone without a path.
fn push_candidates(files: Vec<String>, paths: &Value, out: &mut Vec<String>) {
    let paths = split_on(paths, &[',', ':', ';']);
    if paths.is_empty() {
        out.extend(files);
        return;
    }
    for path in &paths {
        for file in &files {
            out.push(Path::new(path).join(file).display().to_string());
        }
    }
}

/// `_split_on`: a string split on each of `seps`, empty pieces kept; a list, each of its items.
fn split_on(value: &Value, seps: &[char]) -> Vec<String> {
    match value.kind() {
        ValueKind::String => text_of(value).split(seps).map(str::to_string).collect(),
        ValueKind::Seq => value
            .try_iter()
            .map(|items| items.flat_map(|v| split_on(&v, seps)).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests;
