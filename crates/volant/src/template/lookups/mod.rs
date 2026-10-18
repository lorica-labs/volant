// SPDX-License-Identifier: GPL-3.0-or-later
//! `lookup(...)`: the lookup plugins the engine has, as one global function.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use minijinja::value::{Kwargs, Rest, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State, Value};

mod basic;
mod fileglob;
mod first_found;
mod template;

use basic::{inventory_hostnames, varnames};
use fileglob::fileglob;
use first_found::{first_found, relative_stack};
use template::{find_template, render_template};

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

#[cfg(test)]
mod tests;
