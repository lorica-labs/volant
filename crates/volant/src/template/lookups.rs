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
            // Measured on ansible-core 2.19.12: an undefined term or option is an undefined
            // read (`lookup('env', nope | lower)`, `lookup('vars', 'x', default=nope | lower)`).
            let undefined_option = kwargs
                .args()
                .any(|k| kwargs.peek::<Value>(k).is_ok_and(|v| v.is_undefined()));
            if undefined_option || terms.iter().any(super::holds_undefined) {
                return Err(Error::from(ErrorKind::UndefinedError));
            }
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
    if matches!(name, "fileglob" | "ansible.builtin.fileglob") {
        return fileglob(state, terms, kwargs, base_dir);
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
    Ok(scalar_or_list(results))
}

/// The shape every lookup plugin's result takes once it has gone through `lookup()`, not
/// `query()`: no result is the empty string, one result is that value with its own type, several
/// are a list. Every plugin answers through this same rule, `fileglob` included - measured on
/// ansible-core 2.19.12: `lookup('fileglob', 'files/a.txt')` with one match answers a bare string
/// (`type_debug` -> `str`), not a one-element list.
fn scalar_or_list(mut results: Vec<Value>) -> Value {
    match results.len() {
        0 => Value::from(""),
        1 => results.remove(0),
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

/// `lookup('fileglob', pattern...)`, `plugins/lookup/fileglob.py`, read on the dev machine
/// (ansible-core 2.19.12): a pattern with no directory of its own is globbed under `files/` of
/// each entry of `ansible_search_path`, then under the entry itself, the first entry with a
/// match winning; a pattern that already names a directory - an absolute one, measured on the
/// two static `with_fileglob` patterns of the `airgap` role, or a relative one under a search
/// entry - is globbed at that directory directly. Only files match, several terms concatenate
/// into one list before it is sorted whole. The result then goes through the same shaping every
/// other plugin's does (`scalar_or_list`): no match anywhere is the empty string, one match is a
/// bare string - measured on ansible-core 2.19.12, `lookup('fileglob', 'files/a.txt')` with a
/// single match answers `type_debug` -> `str`, not a one-element list - and several are a list.
///
/// The result is controller content, not tainted: a matched name is a path the pattern's author
/// named and the controller's filesystem confirmed, the same standing `lookup('first_found')`
/// gives the path it finds. Nothing here reads a managed host.
fn fileglob(
    state: &State,
    terms: &[Value],
    kwargs: Kwargs,
    base_dir: &Path,
) -> Result<Value, Error> {
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
    out.sort();
    Ok(scalar_or_list(out.into_iter().map(Value::from).collect()))
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
/// reference's own `glob.glob` answers the same way for a directory that is not there.
fn glob_files(dir: &Path, pattern: &str) -> Vec<String> {
    let Ok(re) = regex::Regex::new(&fnmatch_to_regex(pattern)) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| re.is_match(name))
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
    let subdir = state
        .lookup(WITH_SUBDIR)
        .and_then(|v| v.as_str().map(str::to_string));
    for name in &candidates {
        if let Some(found) = relative_stack(&search, subdir.as_deref(), name)
            .into_iter()
            .find(|p| p.exists())
        {
            return Ok(Value::from(found_path(found.display().to_string())?));
        }
    }
    if options.skip {
        return Ok(Value::from(Vec::<Value>::new()));
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

    /// A plain directory of files, for the absolute-pattern case `lookup('fileglob')` takes
    /// straight: no role, no search path, just a controller directory the pattern already names.
    struct Dir {
        path: PathBuf,
    }

    impl Dir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("volant-fileglob-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn write(&self, name: &str) -> &Self {
            std::fs::write(self.path.join(name), "x").unwrap();
            self
        }

        /// A directory (not a file) under the fixture, so a pattern that matches its name still
        /// must not match it as a result.
        fn mkdir(&self, name: &str) -> &Self {
            std::fs::create_dir_all(self.path.join(name)).unwrap();
            self
        }

        fn pattern(&self, glob: &str) -> String {
            format!("{}/{glob}", self.path.display())
        }

        fn full(&self, name: &str) -> String {
            self.path.join(name).display().to_string()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// The two static `with_fileglob` patterns of the `airgap` role, against a fixture
    /// directory: `k3s-selinux*.rpm` and `*.tar.gz`, both rendered to an absolute pattern the
    /// way `"{{ airgap_dir }}/..."` does. `container-selinux-1.rpm` proves the pattern is
    /// matched and not just the extension, a directory whose own name matches the pattern
    /// (`k3s-selinux-dir.rpm`, `images-dir.tar.gz`) proves a directory never matches, and the
    /// result is sorted. Two matches each, so both stay a list under `scalar_or_list`.
    ///
    /// What would make this red: the absolute pattern read as a search-path entry instead of
    /// globbed directly, a directory counted as a match, or the result left in read-dir order.
    #[test]
    fn fileglob_matches_the_two_static_airgap_patterns() {
        let dir = Dir::new("airgap");
        dir.write("container-selinux-1.rpm")
            .write("k3s-selinux-1.rpm")
            .write("k3s-selinux-1.el8.rpm")
            .write("other.txt")
            .mkdir("k3s-selinux-dir.rpm");
        let t = Templar::new(dir.path.clone());
        let rpm = dir.pattern("k3s-selinux*.rpm");
        assert_eq!(
            t.render(
                &format!("{{{{ lookup('fileglob', '{rpm}') }}}}"),
                &Map::new()
            )
            .unwrap(),
            json!([
                dir.full("k3s-selinux-1.el8.rpm"),
                dir.full("k3s-selinux-1.rpm")
            ])
        );

        dir.write("images-2.tar.gz")
            .write("images-1.tar.gz")
            .mkdir("images-dir.tar.gz");
        let images = dir.pattern("*.tar.gz");
        assert_eq!(
            t.render(
                &format!("{{{{ lookup('fileglob', '{images}') }}}}"),
                &Map::new()
            )
            .unwrap(),
            json!([dir.full("images-1.tar.gz"), dir.full("images-2.tar.gz")])
        );
    }

    /// A pattern with no directory of its own: `files/` of the search path wins over the entry
    /// itself when both would match, and the entry itself only when `files/` has nothing at
    /// all - measured behaviour of `plugins/lookup/fileglob.py`, read on the dev machine. Each
    /// case has exactly one match, so `scalar_or_list` answers a bare string, not a list.
    ///
    /// What would make this red: reading the entry itself before `files/`, or merging matches
    /// from both instead of stopping at the first that has any.
    #[test]
    fn fileglob_prefers_files_over_the_entry_itself() {
        let role = Role::new("fileglob");
        role.write("role/files/only-in-files.conf", "f")
            .write("role/only-in-role.conf", "r")
            .write("role/both.conf", "role's own")
            .write("role/files/both.conf", "files' own");
        let t = templar(&role.dir);
        let vars = role.vars();
        assert_eq!(
            t.render("{{ lookup('fileglob', 'only-in-files.conf') }}", &vars)
                .unwrap(),
            json!(
                role.role()
                    .join("files/only-in-files.conf")
                    .display()
                    .to_string()
            )
        );
        assert_eq!(
            t.render("{{ lookup('fileglob', 'only-in-role.conf') }}", &vars)
                .unwrap(),
            json!(role.role().join("only-in-role.conf").display().to_string())
        );
        assert_eq!(
            t.render("{{ lookup('fileglob', 'both.conf') }}", &vars)
                .unwrap(),
            json!(role.role().join("files/both.conf").display().to_string())
        );
    }

    /// No match anywhere is the empty string, never an error - the same `scalar_or_list` gives
    /// `lookup('env')` and every other plugin with nothing to answer, and what the reference's
    /// own `ret = []` becomes once `lookup()` (not `query()`) joins it.
    #[test]
    fn fileglob_with_no_match_is_the_empty_string_not_an_error() {
        let role = Role::new("fileglob-empty");
        let t = templar(&role.dir);
        assert_eq!(
            t.render("{{ lookup('fileglob', 'nope*.txt') }}", &role.vars())
                .unwrap(),
            json!("")
        );
    }

    /// `ansible.builtin.fileglob` answers exactly as the bare name does (A9: minijinja accepts
    /// a qualified lookup name the same as a qualified filter or test name), a bare string here
    /// too since there is exactly one match.
    #[test]
    fn fileglob_answers_under_its_ansible_builtin_alias_too() {
        let role = Role::new("fileglob-alias");
        role.write("role/files/x.conf", "x");
        let t = templar(&role.dir);
        assert_eq!(
            t.render(
                "{{ lookup('ansible.builtin.fileglob', 'x.conf') }}",
                &role.vars()
            )
            .unwrap(),
            json!(role.role().join("files/x.conf").display().to_string())
        );
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
