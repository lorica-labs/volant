// SPDX-License-Identifier: GPL-3.0-or-later
//! `lookup(...)`: the lookup plugins the engine has, as one global function.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use minijinja::value::{Kwargs, Rest, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State, Value};

use super::{Context, FileRender, Templar, TemplateError, Vars};

pub fn register(env: &mut Environment<'static>, base_dir: PathBuf) {
    env.add_function(
        "lookup",
        move |state: &State, name: String, terms: Rest<Value>, kwargs: Kwargs| {
            lookup(state, &name, &terms, kwargs, &base_dir)
        },
    );
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, msg.into())
}

/// `lookup('env'|'file'|'vars'|'pipe'|'template', term...)`. One term gives a scalar, several
/// give a list. `first_found` reads all its terms as one search.
fn lookup(
    state: &State,
    name: &str,
    terms: &[Value],
    kwargs: Kwargs,
    base_dir: &Path,
) -> Result<Value, Error> {
    if matches!(name, "first_found" | "ansible.builtin.first_found") {
        return first_found(state, terms, kwargs, base_dir);
    }
    let default_value: Option<Value> = kwargs.get::<Option<Value>>("default")?;
    kwargs.assert_all_used()?;
    // Whatever a lookup read at run time is data: a file's text, a command's output, an
    // environment variable. Measured on ansible-core 2.19.12, all three come back carrying the
    // tag that stops the engine templating them again, and the three display the same
    // `{{ 1 + 1 }}` the file that held it did. `vars` is not one of them: it hands back a value
    // the context already holds, and reading an untrusted name through it taints on the same
    // path a bare read does.
    let taint = || {
        if let Some(sink) = state.lookup(super::TAINT_KEY)
            && let Some(sink) = sink.downcast_object_ref::<super::TaintSink>()
        {
            sink.taint();
        }
    };
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
                let path = if term_text.starts_with('/') {
                    PathBuf::from(&term_text)
                } else {
                    base_dir.join(&term_text)
                };
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    invalid(format!(
                        "could not locate file in lookup: {}: {e}",
                        path.display()
                    ))
                })?;
                taint();
                Value::from(text.trim_end_matches(['\r', '\n']))
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
                render_template(state, &term_text, &path, &text, base_dir)?
            }
            other => {
                return Err(invalid(format!(
                    "lookup plugin ({other}) is not available yet"
                )));
            }
        };
        results.push(found);
    }
    Ok(match results.len() {
        0 => Value::from(""),
        1 => results.remove(0),
        _ => Value::from(results),
    })
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
) -> Result<Value, Error> {
    let depth = TEMPLATE_DEPTH.get();
    if depth >= MAX_TEMPLATE_DEPTH {
        return Err(invalid(format!(
            "template lookup: maximum recursion depth exceeded rendering {name}"
        )));
    }
    TEMPLATE_DEPTH.set(depth + 1);
    let out = render_template_at(state, name, path, text, base_dir);
    TEMPLATE_DEPTH.set(depth);
    out.map(Value::from).map_err(|e| lookup_error(name, &e))
}

fn render_template_at(
    state: &State,
    name: &str,
    path: &Path,
    text: &str,
    base_dir: &Path,
) -> Result<String, TemplateError> {
    let templar = Templar::new(base_dir.to_path_buf());
    let root = state.lookup(super::CONTEXT_KEY);
    let ctx = root
        .as_ref()
        .and_then(|r| r.downcast_object_ref::<Context>());
    let mut map = ctx.map(|c| c.vars.clone()).unwrap_or_default();
    let mut untrusted: BTreeSet<String> = ctx.map(|c| c.untrusted.clone()).unwrap_or_default();
    let hostvars = ctx.and_then(|c| c.hostvars.as_ref());
    let shared = ctx.and_then(|c| c.shared.as_ref());
    let untrusted_hosts = ctx.map(|c| &c.untrusted_hosts);

    let read = templar
        .env
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
    let vars = Vars {
        map: &map,
        hostvars,
        shared,
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
) -> Result<Value, Error> {
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
    for name in &candidates {
        if let Some(found) = search.iter().map(|dir| dir.join(name)).find(|p| p.exists()) {
            return Ok(Value::from(found.display().to_string()));
        }
    }
    if options.skip {
        return Ok(Value::from(Vec::<Value>::new()));
    }
    Err(invalid("No file was found when using first_found."))
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
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use serde_json::{Map, Value, json};

    use super::super::{Templar, Vars};

    /// A role laid out the way the reference measured one: `templates/`, `files/`, `tasks/`, and
    /// `ansible_search_path` naming the role, its `tasks/` and the playbook directory, in that
    /// order.
    struct Role {
        dir: PathBuf,
    }

    impl Role {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("volant-lookups-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for sub in ["role/templates", "role/files", "role/tasks"] {
                std::fs::create_dir_all(dir.join(sub)).unwrap();
            }
            Self { dir }
        }

        fn write(&self, rel: &str, text: &str) -> &Self {
            std::fs::write(self.dir.join(rel), text).unwrap();
            self
        }

        fn role(&self) -> PathBuf {
            self.dir.join("role")
        }

        fn vars(&self) -> Map<String, Value> {
            let role = self.role();
            let search = [role.clone(), role.join("tasks"), self.dir.clone()];
            let search: Vec<String> = search.iter().map(|p| p.display().to_string()).collect();
            json!({"ansible_search_path": search, "who": "me"})
                .as_object()
                .unwrap()
                .clone()
        }
    }

    impl Drop for Role {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn templar(base: &Path) -> Templar {
        Templar::new(base.to_path_buf())
    }

    /// Measured on ansible-core 2.19.12: with `templates/t.j2` holding `{{ '{{ 1 + 1 }}' }}`,
    /// `via_lookup: "{{ lookup('template', 't.j2') }}"` shows `{{ 1 + 1 }}` under both
    /// `debug: var: via_lookup` and `debug: msg: "{{ via_lookup }}"`. What `lookup('template')`
    /// hands back is never rendered again, the same as `lookup('file')`.
    ///
    /// What would make this red: the `template` arm not marking its result as data. The
    /// variable's own resolution and the argument's extra passes would then render the braces,
    /// and both reads would show `2`.
    #[test]
    fn what_a_template_lookup_rendered_is_never_rendered_again() {
        let role = Role::new("trust");
        role.write("role/templates/t.j2", "{{ '{{ 1 + 1 }}' }}");
        let t = templar(&role.dir);
        let mut map = role.vars();
        map.insert(
            "via_lookup".into(),
            json!("{{ lookup('template', 't.j2') }}"),
        );
        // `debug: var: via_lookup` reads the name out of the resolved map.
        let (resolved, untrusted) = t.resolve_vars_tainted(&map);
        assert_eq!(resolved["via_lookup"], json!("{{ 1 + 1 }}"));
        assert!(untrusted.contains("via_lookup"), "{untrusted:?}");
        // `debug: msg: "{{ via_lookup }}"` renders an argument against it.
        let vars = Vars {
            map: &resolved,
            hostvars: None,
            shared: None,
            untrusted: Some(&untrusted),
            untrusted_hosts: None,
        };
        assert_eq!(
            t.render("{{ via_lookup }}", vars).unwrap(),
            json!("{{ 1 + 1 }}")
        );
        // And an argument that calls the lookup itself, before any variable holds it.
        assert_eq!(
            t.render("{{ lookup('template', 't.j2') }}", &map).unwrap(),
            json!("{{ 1 + 1 }}")
        );
    }

    /// The measured `lookup('template', 'snip.j2')` in a role: `templates/snip.j2` is found
    /// under the role's own entry of `ansible_search_path`, rendered with the template module's
    /// default options (the final newline kept), against the task's variables.
    #[test]
    fn a_template_lookup_renders_the_role_s_template() {
        let role = Role::new("snip");
        role.write("role/templates/snip.j2", "from role templates {{ who }}\n");
        let t = templar(&role.dir);
        assert_eq!(
            t.render("{{ lookup('template', 'snip.j2') }}", &role.vars())
                .unwrap(),
            json!("from role templates me\n")
        );
        // `<entry>/<name>` is the second place looked at, after `<entry>/templates/<name>`.
        role.write("role/bare.j2", "bare {{ who }}");
        assert_eq!(
            t.render("{{ lookup('template', 'bare.j2') }}", &role.vars())
                .unwrap(),
            json!("bare me")
        );
    }

    /// A template lookup whose template reads a name nobody set fails the way any undefined
    /// read does, and one that names no file says which.
    #[test]
    fn a_template_lookup_fails_on_an_undefined_name_and_a_missing_file() {
        let role = Role::new("fail");
        role.write("role/templates/u.j2", "{{ nobody_set_this }}");
        let t = templar(&role.dir);
        let err = t
            .render("{{ lookup('template', 'u.j2') }}", &role.vars())
            .unwrap_err();
        assert!(err.is_undefined(), "{err}");
        assert!(err.0.contains("in the template u.j2"), "{err}");
        let err = t
            .render("{{ lookup('template', 'none.j2') }}", &role.vars())
            .unwrap_err();
        assert!(
            err.0
                .contains("the template file none.j2 could not be found for the lookup"),
            "{err}"
        );
    }

    /// A template that looks itself up fails the task instead of overflowing the stack.
    #[test]
    fn a_template_that_looks_itself_up_fails() {
        let role = Role::new("loop");
        role.write(
            "role/templates/self.j2",
            "{{ lookup('template', 'self.j2') }}",
        );
        let err = templar(&role.dir)
            .render("{{ lookup('template', 'self.j2') }}", &role.vars())
            .unwrap_err();
        assert!(err.0.contains("recursion"), "{err}");
    }

    /// The five `first_found` lines measured on ansible-core 2.19.12 in a role holding
    /// `files/second.txt` and `templates/snip.j2`. Each name is tried against each entry of
    /// `ansible_search_path` as written: nothing adds `files/` or `templates/`.
    ///
    /// What would make this red: `first_found` looking under `files/` of its own accord, which
    /// finds the bare `second.txt` the reference does not.
    #[test]
    fn first_found_tries_each_name_against_the_search_path_only() {
        let role = Role::new("ff");
        role.write("role/files/second.txt", "2")
            .write("role/templates/snip.j2", "x");
        let t = templar(&role.dir);
        let vars = role.vars();
        let found = role.role().join("files/second.txt").display().to_string();
        let r = |text: &str| t.render(text, &vars);
        let not_found = |text: &str| {
            let err = r(text).unwrap_err().0;
            assert!(
                err.contains("No file was found when using first_found."),
                "{text}: {err}"
            );
        };
        not_found("{{ lookup('first_found', ['nope.txt', 'second.txt']) }}");
        assert_eq!(
            r("{{ lookup('first_found', ['nope.txt', 'files/second.txt']) }}").unwrap(),
            json!(found)
        );
        assert_eq!(
            r("{{ lookup('first_found', {'files': ['nope.txt', 'second.txt'], 'paths': ['files']}) }}")
                .unwrap(),
            json!(found)
        );
        not_found("{{ lookup('first_found', ['nope.j2', 'snip.j2']) }}");
        // `skip: true` turns the failure into an empty list, in the mapping or as a keyword.
        assert_eq!(
            r("{{ lookup('first_found', {'files': ['nope.txt'], 'skip': true}) }}").unwrap(),
            json!([])
        );
        assert_eq!(
            r("{{ lookup('first_found', 'nope.txt', skip=true) }}").unwrap(),
            json!([])
        );
        // An absolute name is taken as it stands, and `,` `;` split a name list.
        assert_eq!(
            r(&format!(
                "{{{{ lookup('first_found', 'nope.txt;{found}') }}}}"
            ))
            .unwrap(),
            json!(found)
        );
    }

    /// `_process_terms` joins a string term to the plugin's `paths` option, so the keyword
    /// applies to plain names as the mapping's `paths` does. And a mapping term replaces the
    /// options whole, so one without `skip` turns a `skip=true` keyword back off.
    ///
    /// What would make this red: `paths=` read only for a mapping (the name is then tried bare,
    /// and `second.txt` is not found), or a mapping that keeps the keyword's `skip`.
    #[test]
    fn first_found_joins_the_paths_keyword_and_a_mapping_resets_the_options() {
        let role = Role::new("ffopt");
        role.write("role/files/second.txt", "2");
        let t = templar(&role.dir);
        let vars = role.vars();
        let found = role.role().join("files/second.txt").display().to_string();
        assert_eq!(
            t.render(
                "{{ lookup('first_found', 'second.txt', paths=['files']) }}",
                &vars
            )
            .unwrap(),
            json!(found)
        );
        let err = t
            .render(
                "{{ lookup('first_found', {'files': ['nope.txt']}, skip=true) }}",
                &vars,
            )
            .unwrap_err();
        assert!(
            err.0.contains("No file was found when using first_found."),
            "{err}"
        );
    }

    /// The reference templates a variable when it is read, so a template read through the
    /// lookup sees `a: "{{ b }}"` as `1` even while the task's variables are still being
    /// resolved. Here the lookup can run inside that resolution, against a map that still holds
    /// `{{ b }}`: the names the template reads are resolved first. The result is data all the
    /// same, and a name from a managed host is not rendered on the way.
    ///
    /// What would make this red: the template rendered against the map as it stands, which
    /// writes `{{ b }}` into a result nothing renders again; or the host's name rendered too.
    #[test]
    fn a_template_lookup_reads_a_chained_variable_resolved() {
        let role = Role::new("chain");
        role.write("role/templates/chain.j2", "{{ a }}");
        let t = templar(&role.dir);
        let mut map = role.vars();
        map.insert("a".into(), json!("{{ b }}"));
        map.insert("b".into(), json!(1));
        map.insert("via".into(), json!("{{ lookup('template', 'chain.j2') }}"));
        let (resolved, untrusted) = t.resolve_vars_tainted(&map);
        assert_eq!(resolved["via"], json!("1"));
        assert!(untrusted.contains("via"), "{untrusted:?}");

        let from_host = BTreeSet::from(["a".to_string()]);
        let vars = Vars {
            map: &map,
            hostvars: None,
            shared: None,
            untrusted: Some(&from_host),
            untrusted_hosts: None,
        };
        assert_eq!(
            t.render("{{ lookup('template', 'chain.j2') }}", vars)
                .unwrap(),
            json!("{{ b }}")
        );
    }

    /// Resolving the names a template reads must not fail a lookup the reference renders: its
    /// variables are templated when read, so a name in a branch that never runs, or behind
    /// `default`, never fails. One that cannot be resolved reads as undefined here, and only a
    /// read that happens fails, as an undefined read.
    ///
    /// What would make this red: the resolution's error propagated, which fails the first two
    /// lookups with `undefined value` where the reference prints `ok` and `none`.
    #[test]
    fn a_name_that_cannot_be_resolved_fails_only_when_it_is_read() {
        let role = Role::new("lazy");
        role.write(
            "role/templates/branch.j2",
            "{% if use_proxy %}{{ proxy }}{% endif %}ok",
        )
        .write("role/templates/default.j2", "{{ proxy | default('none') }}")
        .write("role/templates/defined.j2", "{{ proxy is defined }}")
        .write("role/templates/read.j2", "{{ proxy }}");
        let t = templar(&role.dir);
        let mut map = role.vars();
        map.insert("use_proxy".into(), json!(false));
        map.insert("proxy".into(), json!("{{ proxy_host }}:3128"));
        let r = |name: &str| t.render(&format!("{{{{ lookup('template', '{name}') }}}}"), &map);
        assert_eq!(r("branch.j2").unwrap(), json!("ok"));
        assert_eq!(r("default.j2").unwrap(), json!("none"));
        assert_eq!(r("defined.j2").unwrap(), json!("False"));
        let err = r("read.j2").unwrap_err();
        assert!(err.is_undefined(), "{err}");
    }

    /// `plugins/lookup/template.py` gives the template `template_path`, `template_fullpath` and,
    /// unless the variable exists, `ansible_managed`, as the `template` action does: the roles
    /// measured write `{{ ansible_managed | comment }}` at the top of their templates.
    ///
    /// What would make this red: the lookup rendering against the caller's variables alone
    /// (`ansible_managed` undefined), or overwriting an `ansible_managed` the playbook set.
    #[test]
    fn a_template_lookup_sets_the_template_variables() {
        let role = Role::new("tvars");
        role.write(
            "role/templates/m.j2",
            "{{ ansible_managed | comment }}\n{{ template_path }} {{ template_fullpath }}\n",
        );
        let t = templar(&role.dir);
        let full = role.role().join("templates/m.j2").display().to_string();
        assert_eq!(
            t.render("{{ lookup('template', 'm.j2') }}", &role.vars())
                .unwrap(),
            json!(format!("#\n# Ansible managed\n#\nm.j2 {full}\n"))
        );
        let mut map = role.vars();
        map.insert("ansible_managed".into(), json!("by hand"));
        assert_eq!(
            t.render("{{ lookup('template', 'm.j2') }}", &map).unwrap(),
            json!(format!("#\n# by hand\n#\nm.j2 {full}\n"))
        );
    }
}
