// SPDX-License-Identifier: GPL-3.0-or-later
//! `lookup('template')`: a template file rendered against the caller's variables.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use minijinja::value::Kwargs;
use minijinja::{Error, ErrorKind, State, Value};

use super::{invalid, search_path};
use crate::bridge::Bridge;
use crate::template::{Context, FileRender, Templar, TemplateError, Vars};

/// `<entry>/templates/<name>`, then `<entry>/<name>`, for each entry of the search path.
pub(super) fn find_template(state: &State, name: &str, base_dir: &Path) -> Result<PathBuf, Error> {
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
pub(super) const MAX_TEMPLATE_DEPTH: usize = 32;

/// The options `lookup('template')` takes: the four delimiters and `trim_blocks`, read the way
/// the reference reads them, and nothing else.
pub(super) fn options(kwargs: &Kwargs) -> Result<FileRender, Error> {
    let mut options = FileRender::default();
    let syntax = &mut options.syntax;
    for (name, field) in [
        ("variable_start_string", &mut syntax.variable_start),
        ("variable_end_string", &mut syntax.variable_end),
        ("comment_start_string", &mut syntax.comment_start),
        ("comment_end_string", &mut syntax.comment_end),
    ] {
        if let Some(value) = kwargs.get::<Option<String>>(name)? {
            *field = value;
        }
    }
    syntax.check_starts().map_err(invalid)?;
    if let Some(trim) = kwargs.get::<Option<bool>>("trim_blocks")? {
        options.trim_blocks = trim;
    }
    Ok(options)
}

/// The file rendered once, with the options of the call (the `template` module's defaults when
/// it gave none), against the variables
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
pub(super) fn render_template(
    state: &State,
    name: &str,
    path: &Path,
    text: &str,
    base_dir: &Path,
    bridge: Option<&Arc<Bridge>>,
    options: &FileRender,
) -> Result<Value, Error> {
    let depth = TEMPLATE_DEPTH.get();
    if depth >= MAX_TEMPLATE_DEPTH {
        return Err(invalid(format!(
            "template lookup: maximum recursion depth exceeded rendering {name}"
        )));
    }
    TEMPLATE_DEPTH.set(depth + 1);
    // The lookup's own search path for includes: the file's directory, then `<p>/templates` and
    // `<p>` for each entry of `ansible_search_path`.
    let mut options = options.clone();
    options.include_path = path.parent().map(Path::to_path_buf).into_iter().collect();
    options
        .include_path
        .extend(crate::template::widened(search_path(state, base_dir)));
    let out = render_template_at(state, name, path, text, base_dir, bridge, &options);
    if let Err(e) = &out
        && crate::template::is_not_found(&e.0)
    {
        super::super::include::note_asked_in(&e.0, path);
    }
    TEMPLATE_DEPTH.set(depth);
    // A template that writes nothing at all, a comment alone say, is `None` there: the engine
    // joins no piece of output into no value, and has no output to give the file's last newline
    // back to. One that prints an empty string is `''`, and plain text is itself.
    out.map(|(s, node)| {
        if let Some(value) = node {
            return value;
        }
        let written = if text.ends_with('\n') {
            s.strip_suffix('\n').unwrap_or(&s)
        } else {
            &s
        };
        let tags = text.contains("{%") || text.contains("{#");
        if written.is_empty() && tags && !text.contains("{{") {
            Value::from(())
        } else {
            Value::from(s)
        }
    })
    .map_err(|e| lookup_error(name, &e))
}

pub(super) fn render_template_at(
    state: &State,
    name: &str,
    path: &Path,
    text: &str,
    base_dir: &Path,
    bridge: Option<&Arc<Bridge>>,
    options: &FileRender,
) -> Result<(String, Option<Value>), TemplateError> {
    let templar = Templar::with_bridge(base_dir.to_path_buf(), bridge.cloned());
    let root = state.lookup(crate::template::CONTEXT_KEY);
    let ctx = root
        .as_ref()
        .and_then(|r| r.downcast_object_ref::<Context>());
    let mut map = ctx.map(Context::flat_vars).unwrap_or_default();
    let mut untrusted: BTreeSet<String> =
        ctx.map(Context::untrusted_for_nested).unwrap_or_default();
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
        if untrusted.contains(&var) || !map.get(&var).is_some_and(crate::template::holds_template) {
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
                base: None,
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
    // The lookup renders no destination, which the reference leaves `None`.
    map.insert("template_destpath".into(), serde_json::Value::Null);
    for (name, value) in crate::template::file_vars(path) {
        map.insert(name.into(), value);
    }
    let named = [
        "template_path",
        "template_fullpath",
        "template_destpath",
        "template_host",
        "template_uid",
        "template_mtime",
        "template_run_date",
    ];
    for name in named {
        untrusted.remove(name);
    }
    // The names just written beat a gathered fact of the same name, which would otherwise
    // answer first from its own layer.
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
        base: None,
    };
    templar.render_file_node(text, vars, options)
}

pub(super) fn lookup_error(name: &str, e: &TemplateError) -> Error {
    // An include the file asked for and the loader did not find is the render's own error, not
    // the lookup's: the reference words it as it is.
    if crate::template::is_not_found(&e.0) {
        return crate::template::bridged::raise(e.0.clone());
    }
    if e.is_undefined() {
        // Kept an undefined error, so the caller's own render reports it as one.
        let detail =
            e.0.strip_prefix(crate::template::UNDEFINED)
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
