// SPDX-License-Identifier: GPL-3.0-or-later
//! What a play is checked for before it runs: `max_fail_percentage`, the gather keywords and its `vars_files`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{RunState, python_type};
use crate::inventory::Host;
use crate::playbook::Play;
use crate::render::Renderer;
use crate::template::{Templar, Vars};
use crate::vars::{Scope, load_vars_file};

/// The play's `max_fail_percentage`, rendered once when the play starts: a batch that loses a
/// larger share of its hosts than this ends the run. A value that is no number is refused here,
/// before the play's banner, like a `serial` that does not render.
pub(super) fn max_fail_percentage(
    play: &Play,
    templar: &Templar,
    vars: &Map<String, Value>,
) -> anyhow::Result<Option<f64>> {
    let Some(raw) = &play.max_fail_percentage else {
        return Ok(None);
    };
    let refuse = |why: String| {
        crate::stats::Refusal::at(
            4,
            format!("Error processing keyword 'max_fail_percentage': {why}"),
        )
    };
    let rendered = templar
        .render_value(raw, vars)
        .map_err(|err| refuse(err.0))?;
    let number = match &rendered {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    number
        .filter(|n| n.is_finite())
        .map(Some)
        .ok_or_else(|| refuse(format!("The value {rendered} is not a number.")))
}

/// The play's `gather_subset`, `gather_timeout` and `fact_path`, checked when the play starts,
/// before its banner: the reference validates a play's keywords then, whether it gathers or not.
/// The values themselves travel as the gathering step's `setup` arguments, see
/// `compile::gather_args`.
pub(super) fn check_gather_keywords(
    play: &Play,
    templar: &Templar,
    vars: &Map<String, Value>,
) -> anyhow::Result<()> {
    let refuse = |key: &str, why: String| {
        crate::stats::Refusal::at(4, format!("Error processing keyword '{key}': {why}"))
    };
    if let Some(raw) = &play.gather_subset {
        let rendered = match templar.render_value(raw, vars) {
            Ok(rendered) => rendered,
            // A variable only a host has (a `vars_files` entry, say) is that host's to render.
            Err(err) if err.0.contains("undefined") => Value::Null,
            Err(err) => return Err(refuse("gather_subset", err.0)),
        };
        let items = match &rendered {
            Value::Null => &[][..],
            Value::Array(items) => items.as_slice(),
            one => std::slice::from_ref(one),
        };
        if let Some(bad) = items.iter().find(|item| !item.is_string()) {
            return Err(refuse(
                "gather_subset",
                format!(
                    "Keyword 'gather_subset' items must be of type 'str', not '{}'.",
                    python_type(bad)
                ),
            ));
        }
    }
    if let Some(raw) = &play.gather_timeout {
        let rendered = match templar.render_value(raw, vars) {
            Ok(rendered) => rendered,
            Err(err) if err.0.contains("undefined") => Value::from(0),
            Err(err) => return Err(refuse("gather_timeout", err.0)),
        };
        let whole = match &rendered {
            Value::Number(n) => n.is_i64() || n.is_u64(),
            Value::String(text) => text.trim().parse::<i64>().is_ok(),
            _ => false,
        };
        if !whole {
            let shown = rendered
                .as_str()
                .map_or_else(|| rendered.to_string(), str::to_string);
            return Err(refuse(
                "gather_timeout",
                format!("The value '{shown}' could not be converted to 'int'."),
            ));
        }
    }
    if let Some(Value::Array(_) | Value::Object(_)) = &play.fact_path {
        let kind = play.fact_path.as_ref().map_or("", python_type);
        return Err(crate::stats::Refusal::at(
            4,
            format!(
                "The field 'fact_path' is supposed to be a string type, however the incoming data structure is a {kind}"
            ),
        ));
    }
    Ok(())
}

/// A variable's boolean, whether the inventory typed it as one or spelled it the way Ansible
/// content spells one (`yes`, `on`, `"true"`).
pub(crate) fn as_bool_value(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => crate::yaml::bool_from_str(s.trim()),
        _ => None,
    }
}

/// `vars_files` paths are templates over play vars and the host's own variables, and Ansible
/// resolves them per host, so two hosts can read two different files. Each rendered path is
/// read once per call.
///
/// Called once per batch, not once per play: measured against ansible-core 2.19.12, a
/// `vars_files` path templated on `ansible_play_batch` reads a different file for each batch
/// (`serial/two.yml`'s shape, extended with a `vars_files` entry), so the reference re-renders
/// and re-reads it per batch rather than once at the play's start. `hosts` is therefore the
/// batch's own live hosts, and `play_hosts` / `batch_hosts` / `all_play_hosts` are the same three
/// lists `ansible_play_hosts`, `ansible_play_batch` and `ansible_play_hosts_all` are served from
/// everywhere else.
///
/// An entry that names nothing is not an error: the reference skips a `vars_files` path that
/// does not exist without a word and runs the play, and warns once for an entry whose template
/// has no value. Every other template failure stops the run, as it does there. A file that is
/// there but cannot be read stops the run too.
#[expect(
    clippy::too_many_arguments,
    reason = "three host lists the reference distinguishes; folding them loses which is which"
)]
pub(super) fn load_play_vars_files(
    play: &Play,
    hosts: &[Host],
    play_hosts: &[String],
    batch_hosts: &[String],
    all_play_hosts: &[String],
    playbook_dir: &Path,
    state: &RunState,
    out: &mut Renderer,
) -> anyhow::Result<HashMap<String, Vec<Map<String, Value>>>> {
    let mut per_host = HashMap::new();
    if play.vars_files.is_empty() {
        return Ok(per_host);
    }
    let mut loaded: HashMap<PathBuf, Map<String, Value>> = HashMap::new();
    // One warning per entry, not one per host: two hosts reading the same unresolvable entry
    // are one complaint.
    let mut warned: HashSet<&String> = HashSet::new();
    for host in hosts {
        let scope = Scope {
            play_vars: play.vars.clone(),
            play_hosts: play_hosts.to_vec(),
            batch_hosts: batch_hosts.to_vec(),
            all_play_hosts: all_play_hosts.to_vec(),
            ..Scope::default()
        };
        let (layers, untrusted, untrusted_hosts) = {
            let mut store = state.vars.lock().expect("vars lock");
            let untrusted = store.untrusted_of(&host.name, &scope);
            let untrusted_hosts = store.untrusted_hosts();
            let layers = store.layered_for_host(&host.name, &scope);
            (layers, untrusted, untrusted_hosts)
        };
        let (resolved, untrusted) = state.templar.resolve_vars_tainted(Vars {
            map: &layers.map,
            hostvars: None,
            shared: Some(&layers.shared),
            facts: Some(&layers.facts),
            untrusted: Some(&untrusted),
            untrusted_hosts: Some(&untrusted_hosts),
        });
        let vars = Vars {
            map: &resolved,
            hostvars: None,
            shared: Some(&layers.shared),
            facts: Some(&layers.facts),
            untrusted: Some(&untrusted),
            untrusted_hosts: Some(&untrusted_hosts),
        };
        let mut files = Vec::new();
        for raw in &play.vars_files {
            let rendered = match state.templar.render(raw, vars) {
                Ok(rendered) => rendered,
                // Only a variable without a value is recoverable. Every other template failure
                // stops the run there, and swallowing them all as one undefined variable both
                // named the wrong cause and let a broken playbook exit 0.
                Err(err) if err.is_undefined() => {
                    if warned.insert(raw) {
                        out.warning(
                            "skipping vars_files item due to an undefined variable",
                            false,
                        );
                    }
                    continue;
                }
                Err(err) => anyhow::bail!("rendering vars_files entry {raw}: {err}"),
            };
            // Exit 4, the reference's own code for a playbook it cannot make sense of, measured
            // alongside the wording. A template that fails to render stays at 1, also measured:
            // the reference splits those two the same way.
            let path = rendered.as_str().ok_or_else(|| {
                crate::stats::Refusal::at(
                    4,
                    format!(
                        "Invalid `vars_files` value of type '{}'. A `vars_files` value should \
                         either be a string or list of strings.",
                        python_type(&rendered)
                    ),
                )
            })?;
            let path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                playbook_dir.join(path)
            };
            // `exists` answers "no" both for a path that is not there and for one it cannot
            // stat, a parent directory refusing access included. The reference asks the same
            // question the same way and skips either without a word, so both stay a skip here.
            if !path.exists() {
                continue;
            }
            files.push(if let Some(file) = loaded.get(&path) {
                file.clone()
            } else {
                // Exit 4 as well: measured, a `vars_files` entry naming a file whose YAML
                // does not parse stops the reference with the same code a broken playbook
                // gets.
                // A vaulted file the run cannot read is exit 1 in the reference.
                let file = load_vars_file(&path).map_err(|err| {
                    if err.is::<crate::vars::VaultFile>() {
                        crate::stats::Refusal::at(1, err)
                    } else {
                        crate::stats::Refusal::or(4, err)
                    }
                })?;
                loaded.insert(path, file.clone());
                file
            });
        }
        per_host.insert(host.name.clone(), files);
    }
    Ok(per_host)
}
