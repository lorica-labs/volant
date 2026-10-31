// SPDX-License-Identifier: GPL-3.0-or-later
//! `lookup(...)`: the lookup plugins the engine has, as one global function.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use minijinja::value::{Kwargs, Rest, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State, Value};

mod basic;
mod fileglob;
mod first_found;
mod loops;
mod template;

use basic::{inventory_hostnames, varnames};
use fileglob::fileglob;
use first_found::{first_found, relative_stack};
use template::{find_template, options as template_options, render_template};

use crate::bridge::{Bridge, PluginKind};
use crate::template::FileRender;

pub fn register(env: &mut Environment<'static>, base_dir: PathBuf, bridge: Option<&Arc<Bridge>>) {
    let bridge = bridge.cloned();
    let call = move |state: &State, name: String, terms: Rest<Value>, kwargs: Kwargs| {
        // Measured on ansible-core 2.19.12: an undefined term or option is an undefined
        // read (`lookup('env', nope | lower)`, `lookup('vars', 'x', default=nope | lower)`).
        // `env` reads its `default` only for a variable that is not set, so an undefined one
        // fails there, in the plugin's words.
        let env = matches!(
            name.as_str(),
            "env" | "ansible.builtin.env" | "ansible.legacy.env"
        );
        let undefined_option = kwargs.args().any(|k| {
            !(env && k == "default") && kwargs.peek::<Value>(k).is_ok_and(|v| v.is_undefined())
        });
        if undefined_option || terms.iter().any(super::holds_undefined) {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        // `ansible.legacy.env` is the engine's `env`, as the reference resolves it.
        let name = match name.strip_prefix("ansible.legacy.") {
            Some(short) if supported(short) => short.to_string(),
            _ => name,
        };
        if super::unknown::active() && differs_between_runs(state, &name, &terms, &base_dir) {
            // Read, so that minijinja sees every option taken.
            for key in kwargs.args() {
                let _ = kwargs.get::<Value>(key);
            }
            let short = name.rsplit('.').next().unwrap_or(&name);
            return Ok(super::unknown::value(&format!("lookup '{short}'")));
        }
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
            Err(err) => return Err(failed(&name, err)),
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

/// A native lookup's failure as the reference words it: `The lookup plugin '<name>' failed:
/// <reason>`. An undefined read and a message already worded in full (see [`super::bridged::raise`])
/// stay as they are.
fn failed(name: &str, err: Error) -> Error {
    if err.kind() == ErrorKind::UndefinedError || super::bridged::said(&err).is_some() {
        return err;
    }
    super::bridged::raise(format!(
        "The lookup plugin '{name}' failed: {}",
        err.detail().unwrap_or_default()
    ))
}

/// Whether lookup `name` may answer otherwise on another run: a command, the environment, a
/// random or generated value, the network, or a file outside the directory the replay runs in.
/// A replay answers such a lookup with the unknown value.
fn differs_between_runs(state: &State, name: &str, terms: &[Value], base_dir: &Path) -> bool {
    let short = name.rsplit('.').next().unwrap_or(name);
    if ["pipe", "env", "password", "lines", "url"].contains(&short) || short.contains("random") {
        return true;
    }
    if short != "file" {
        return false;
    }
    let here = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .unwrap_or_default();
    terms.iter().filter_map(Value::as_str).any(|term| {
        found_file(state, term, base_dir)
            .and_then(|p| std::fs::canonicalize(p).ok())
            .is_some_and(|p| !p.starts_with(&here))
    })
}

/// A lookup that only rearranges its terms.
type Shape = fn(&[Value]) -> Result<Vec<Value>, Error>;

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, msg.into())
}

/// The lookups this release runs, short names; each is also accepted as `ansible.builtin.<name>`.
const SUPPORTED: &[&str] = &[
    "dict",
    "env",
    "file",
    "fileglob",
    "first_found",
    "inventory_hostnames",
    "nested",
    "pipe",
    "subelements",
    "template",
    "together",
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
    // What a loop lookup hands back comes from its terms and is data, as the plugin's is.
    let shaping: Option<Shape> = match name.strip_prefix("ansible.builtin.").unwrap_or(name) {
        "dict" => Some(loops::dict),
        "nested" => Some(loops::nested),
        "subelements" => Some(loops::subelements),
        "together" => Some(loops::together),
        _ => None,
    };
    if let Some(shape) = shaping {
        taint(state);
        return shape(terms);
    }
    if matches!(name, "fileglob" | "ansible.builtin.fileglob") {
        return fileglob(state, terms, base_dir);
    }
    if matches!(
        name,
        "inventory_hostnames" | "ansible.builtin.inventory_hostnames"
    ) {
        return inventory_hostnames(state, terms);
    }
    if matches!(name, "varnames" | "ansible.builtin.varnames") {
        return varnames(state, terms);
    }
    // Peeked first: an undefined `default` is no default to `get`, and `env` still needs it.
    let env_default = kwargs.peek::<Value>("default").ok();
    let default_value: Option<Value> = kwargs.get::<Option<Value>>("default")?;
    // `file`'s two options, as Python's `str.lstrip()` and `str.rstrip()` read them.
    let is_file = matches!(name, "file" | "ansible.builtin.file");
    let strip = |key: &str, default: bool| -> Result<bool, Error> {
        Ok(if is_file {
            kwargs.get::<Option<bool>>(key)?.unwrap_or(default)
        } else {
            default
        })
    };
    let (lstrip, rstrip) = (strip("lstrip", false)?, strip("rstrip", true)?);
    let render_options = matches!(name, "template" | "ansible.builtin.template")
        .then(|| template_options(&kwargs))
        .transpose()?;
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
                match (std::env::var(&term_text), &env_default) {
                    (Ok(set), _) => Value::from(set),
                    (Err(_), None) => Value::from(""),
                    // Undefined, and saying why when something reads it: measured, `lookup('env',
                    // 'X', default=undef()) is defined` is false for an `X` that is not set.
                    (Err(_), Some(default)) if default.is_undefined() => {
                        super::missing::undefined_saying(format!(
                            "The environment variable '{term_text}' is not set."
                        ))
                    }
                    (Err(_), Some(default)) => default.clone(),
                }
            }
            "file" | "ansible.builtin.file" => {
                let Some(path) = found_file(state, &term_text, base_dir) else {
                    return Err(invalid(format!(
                        "Unable to access the file '{term_text}': File not found. Use -vvvvv to see paths searched."
                    )));
                };
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    invalid(format!(
                        "Unable to access the file '{term_text}': {}",
                        read_failure(&path, &e)
                    ))
                })?;
                let text = unvaulted(text)?;
                taint();
                let text = if lstrip { text.trim_start() } else { &text };
                Value::from(if rstrip { text.trim_end() } else { text })
            }
            // The file as it reads once decrypted, a plain one as it is, and no newline dropped:
            // measured, `lookup('unvault', 'f.txt')` of `txt_secret\n` keeps the newline.
            "unvault" | "ansible.builtin.unvault" => {
                let Some(path) = found_file(state, &term_text, base_dir) else {
                    return Err(invalid(format!(
                        "Unable to find file matching \"{term_text}\""
                    )));
                };
                let bytes = std::fs::read(&path)
                    .map_err(|e| invalid(format!("could not read {}: {e}", path.display())))?;
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
                        // The reference raises an undefined-variable error here, with no
                        // `The lookup plugin` part.
                        return Err(super::bridged::raise(format!(
                            "No variable named '{term_text}' was found."
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
                let text =
                    std::fs::read_to_string(&path).map_err(|e| invalid(read_failure(&path, &e)))?;
                let text = unvaulted(text)?;
                taint();
                render_template(
                    state,
                    &term_text,
                    &path,
                    &text,
                    base_dir,
                    bridge,
                    render_options.as_ref().unwrap_or(&FileRender::default()),
                )?
            }
            _ => unreachable!("supported() admitted {name}"),
        };
        results.push(found);
    }
    Ok(results)
}

/// A file's text as the loader's `get_text_file_contents` hands it over: decrypted when it is
/// vaulted. Measured, `lookup('template')` renders a vaulted template.
fn unvaulted(text: String) -> Result<String, Error> {
    if !crate::vault::is_encrypted(text.as_bytes()) {
        return Ok(text);
    }
    let plain = crate::vault::decrypt(text.as_bytes(), &crate::vault::run_secrets())
        .map_err(|e| invalid(e.0))?;
    Ok(String::from_utf8_lossy(&plain).into_owned())
}

/// `find_file_in_search_path`: each entry's `files/<term>`, then its `<term>`.
fn found_file(state: &State, term: &str, base_dir: &Path) -> Option<PathBuf> {
    let search = search_path(state, base_dir);
    relative_stack(&search, Some("files"), term)
        .into_iter()
        .find(|p| p.exists())
}

/// A file that could not be read, as the reference's loader words it: the path, then Python's
/// `[Errno N] reason: 'path'`.
fn read_failure(path: &Path, err: &std::io::Error) -> String {
    let path = path.display();
    let os = err
        .raw_os_error()
        .map_or_else(String::new, |n| format!("[Errno {n}] "));
    let reason = err.to_string();
    let reason = reason.split(" (os error").next().unwrap_or(&reason);
    format!("An error occurred while trying to read the file '{path}': {os}{reason}: '{path}'")
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
