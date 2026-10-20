// SPDX-License-Identifier: GPL-3.0-or-later
//! Roles: where one is found on disk, and what it holds once it has been read.
//!
//! A role is static composition. Nothing here waits for a host: the directory is read while the
//! play is being compiled, its tasks are spliced into the flat step list, and its variables
//! become layers the driver puts under the play's own. `include_role` is a different thing and
//! is not here.
//!
//! The search order and the sentence a missing role is refused with are the reference's own,
//! measured on ansible-core 2.19.12; so is the file-before-directory rule each of a role's
//! sub-directories is read with.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::playbook::{HandlerTask, PlayTask, TaskOrBlock};
use crate::stats::Refusal;

/// Which file of each of a role's directories to read. `main` everywhere unless the entry that
/// named the role said otherwise, which is what `tasks_from` and its three siblings do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleFrom {
    pub tasks: String,
    pub vars: String,
    pub defaults: String,
    pub handlers: String,
}

impl Default for RoleFrom {
    fn default() -> Self {
        Self {
            tasks: "main".to_string(),
            vars: "main".to_string(),
            defaults: "main".to_string(),
            handlers: "main".to_string(),
        }
    }
}

/// One entry of a play's `roles:` list, of a `meta/main.yml` dependency list, or one
/// `import_role` task.
///
/// `params` and `keywords` are two different things, measured apart on ansible-core 2.19.12: a
/// free key on the entry (`{ role: base, p: v }`) is a role parameter and beats a `set_fact`,
/// while `vars:` on the same entry does not. So `vars:` travels in `keywords`, with `when` and
/// the rest, and only free keys are parameters.
#[derive(Debug, Clone)]
pub struct RoleEntry {
    pub name: String,
    pub from: RoleFrom,
    pub params: Map<String, Value>,
    /// The task keywords written on the entry, which every task of the role inherits.
    pub keywords: PlayTask,
    /// Whether the role lends its `defaults` and `vars` to the rest of the play. A role of a
    /// `roles:` list and an `import_role` do unless the statement says `public: false`; an
    /// `include_role` does only when it says `public: true`.
    pub public: bool,
    /// Whether the argument-spec check runs in front of the role: `rolespec_validate`.
    pub validate: bool,
    /// `allow_duplicates: false` on an `include_role` or an `import_role`: the statement does
    /// nothing for a host that has already run an identical one.
    pub unique: bool,
}

impl RoleEntry {
    /// An entry that names a role and nothing else: no keywords, no parameters, the defaults of
    /// a `roles:` list.
    pub fn named(name: &str, from: RoleFrom) -> Self {
        Self {
            name: name.to_string(),
            from,
            params: Map::new(),
            keywords: PlayTask::empty(),
            public: true,
            validate: true,
            unique: false,
        }
    }
}

/// What one role directory holds.
#[derive(Debug, Clone)]
pub struct LoadedRole {
    pub defaults: Map<String, Value>,
    pub vars: Map<String, Value>,
    pub tasks: Vec<TaskOrBlock>,
    /// The role's `handlers/` directory, read with the same file-before-directory rule as its
    /// tasks. They join the play's own handlers at compile time, in front of them - measured on
    /// ansible-core 2.19.12, a role's handler runs before the play's at the same flush.
    pub handlers: Vec<HandlerTask>,
    pub dependencies: Vec<RoleEntry>,
    pub allow_duplicates: bool,
    /// The `collections` of `meta/main.yml`: the only list the role's own tasks search, the
    /// play's being no business of a role.
    pub collections: Vec<String>,
    /// The `options` mapping of the `argument_specs` entry named after the tasks file, when the
    /// role has one. It is what the validation task checks the role's arguments against.
    pub argument_spec: Option<Map<String, Value>>,
    /// The `short_description` of that same entry, which the check's own name carries behind a
    /// dash. Measured on ansible-core 2.19.12 through `--list-tasks`:
    /// `Validating arguments against arg spec 'main' - The spec role`.
    pub argument_spec_description: Option<String>,
}

/// One compiled role instance: the variables its own steps sit on, and the ones it lends to the
/// rest of the play.
#[derive(Debug, Clone, Default)]
pub struct RoleVars {
    pub name: String,
    pub defaults: Map<String, Value>,
    pub vars: Map<String, Value>,
    pub params: Map<String, Value>,
    /// Whether this instance lends `defaults` and `vars` to the rest of the play.
    pub public: bool,
    /// What the role itself says, set aside when the rest of the play's values were first laid
    /// under it, so that a variable another role publishes later can be laid under it again.
    pub own: Option<Box<OwnVars>>,
}

/// The `defaults` and `vars` a role directory holds, before anything was laid under them.
#[derive(Debug, Clone, Default)]
pub struct OwnVars {
    pub defaults: Map<String, Value>,
    pub vars: Map<String, Value>,
    /// What the other roles of the same statement lend: its dependencies, which see each other
    /// and the role that asked for them whether or not the statement is public.
    pub group_defaults: Map<String, Value>,
    pub group_vars: Map<String, Value>,
}

/// Where a role is looked for.
///
/// Measured on ansible-core 2.19.12: with no `roles_path` set, a missing role names
/// `<playbook_dir>/roles`, `~/.ansible/roles`, `/usr/share/ansible/roles`, `/etc/ansible/roles`
/// and `<playbook_dir>`, in that order and separated by colons. A `roles_path` from `ansible.cfg`
/// or from `ANSIBLE_ROLES_PATH` replaces the three middle entries rather than adding to them,
/// and the environment wins over the file - both measured the same way, by reading the list a
/// missing role prints.
#[derive(Debug, Clone, Default)]
pub struct RoleSearch {
    pub paths: Vec<PathBuf>,
    pub collections: Vec<PathBuf>,
}

impl RoleSearch {
    pub fn new(playbook_dir: &Path, config: &Config) -> Self {
        // The reference makes every one of these absolute and folds `.` and `..` out of the text
        // (`unfrackpath`); the search path a role's lookups read is built from them, and so is
        // what an `included:` line shows.
        let playbook_dir = folded(playbook_dir);
        let mut paths = vec![playbook_dir.join("roles")];
        paths.extend(config.roles_path.iter().cloned());
        paths.push(playbook_dir.clone());
        let mut collections = vec![playbook_dir.join("collections")];
        collections.extend(config.collections_path.iter().cloned());
        Self { paths, collections }
    }

    /// The directory a role name points at.
    ///
    /// A three-part name is a collection role and is looked for under
    /// `<collection path>/ansible_collections/<namespace>/<collection>/roles/<role>`; measured,
    /// a `collections/` directory beside the playbook is searched, and the paths a missing one
    /// names are the ordinary ones and not the collection ones.
    pub fn locate(&self, name: &str) -> anyhow::Result<PathBuf> {
        for base in &self.paths {
            let candidate = base.join(name);
            if candidate.is_dir() {
                return Ok(candidate);
            }
        }
        let parts: Vec<&str> = name.split('.').collect();
        if let [namespace, collection, role] = parts[..] {
            for base in &self.collections {
                let candidate = base
                    .join("ansible_collections")
                    .join(namespace)
                    .join(collection)
                    .join("roles")
                    .join(role);
                if candidate.is_dir() {
                    return Ok(candidate);
                }
            }
        }
        // Exit 1, measured, and notably not the 4 the rest of a playbook this engine cannot make
        // sense of gets. It is built with its own code so the blanket code the compiler wraps its
        // failures in cannot bury it.
        Err(Refusal::at(
            1,
            format!(
                "the role '{name}' was not found in {}",
                self.paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(":")
            ),
        ))
    }
}

/// Reads one role directory. `name` is the role as the entry wrote it, for error messages.
pub fn load(path: &Path, name: &str, from: &RoleFrom) -> anyhow::Result<LoadedRole> {
    let defaults = variables(path, name, "defaults", &from.defaults)?;
    let vars = variables(path, name, "vars", &from.vars)?;
    let lookup = |sub, from| Lookup::named(path, name, sub, from, false);
    let tasks = match files(&lookup("tasks", &from.tasks))? {
        None => Vec::new(),
        Some(list) => {
            let mut tasks = Vec::new();
            for file in list {
                tasks.extend(crate::playbook::parse_tasks_file(&file)?);
            }
            tasks
        }
    };
    // A handlers file the entry asked for by name is required the way a tasks file is; `main` is
    // not, because a role with no `handlers/` at all is ordinary.
    let mut handlers = Vec::new();
    for file in files(&lookup("handlers", &from.handlers))?.unwrap_or_default() {
        handlers.extend(crate::playbook::parse_handlers_file(&file)?);
    }
    let (dependencies, allow_duplicates, collections) = meta(path)?;
    let (argument_spec, argument_spec_description) = argument_spec(path, &from.tasks)?;
    Ok(LoadedRole {
        defaults,
        vars,
        tasks,
        handlers,
        dependencies,
        allow_duplicates,
        collections,
        argument_spec,
        argument_spec_description,
    })
}

/// One look into a role's sub-directory: which file the entry asked for, and how to read it.
struct Lookup<'a> {
    role: &'a Path,
    /// The role as the entry wrote it, for the refusal.
    name: &'a str,
    sub: &'a str,
    from: &'a str,
    /// Set when the entry asked for a file by name: measured, `tasks_from: nosuch` refuses the
    /// run with `Could not find specified file in role: tasks/nosuch`, while a role with no
    /// `tasks/main.yml` at all runs its variables and nothing else.
    required: bool,
    /// Only `defaults` and `vars` read a directory; `tasks`, `handlers` and `meta` skip one.
    allow_dir: bool,
}

impl<'a> Lookup<'a> {
    /// A file is required when the entry named one other than `main`.
    fn named(role: &'a Path, name: &'a str, sub: &'a str, from: &'a str, allow_dir: bool) -> Self {
        Self {
            role,
            name,
            sub,
            from,
            required: from != "main",
            allow_dir,
        }
    }
}

/// The files one of a role's sub-directories contributes, or `None` when it has nothing to say.
///
/// This is `_load_role_yaml` of ansible-core 2.19.12, measured. The name is tried with the
/// extensions `.yml`, `.yaml`, `.json` and none; none comes first when the entry named a file and
/// last when it did not. The first candidate that exists wins. A directory is read only where
/// `allow_dir` is set, its files merged in name order, its own sub-directories read the same way.
/// So `defaults/main.yml` beside `defaults/main/` wins and the directory is not read at all.
///
/// Every file found must lie under `<role>/<sub>` once `.` and `..` are folded out of its path.
/// The check is on the text of the path: a symlink inside the sub-directory that points elsewhere
/// is followed, as the reference does. A name such as `../../../x` or `/etc/x` is refused with the
/// reference's own sentence. The path is folded before it is looked up and read, so a `..` after a
/// link to a directory never climbs out through the link.
fn files(look: &Lookup) -> anyhow::Result<Option<Vec<PathBuf>>> {
    let base = look.role.join(look.sub);
    if !base.is_dir() {
        return Ok(None);
    }
    let mut extensions = vec![".yml", ".yaml", ".json"];
    if look.required {
        extensions.insert(0, "");
    } else {
        extensions.push("");
    }
    // Each file is kept twice: as the entry spells it, for the refusal, and with `.` and `..`
    // folded out, which is the path that is looked up and read. The reference folds first, so a `..`
    // after a link to a directory does not climb out through the link.
    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
    for extension in &extensions {
        let mut full = base.join(look.from).into_os_string();
        full.push(extension);
        let shown = PathBuf::from(full);
        let real = folded(&shown);
        if !real.exists() {
            continue;
        }
        if real.is_dir() {
            if !look.allow_dir {
                continue;
            }
            directory_files(&shown, &real, &extensions, &mut found)?;
        } else {
            found.push((shown, real));
        }
        break;
    }
    if found.is_empty() {
        if look.required {
            // Exit 4 comes from the blanket the compiler wraps its failures in, which is the
            // code the reference gives this, measured.
            bail!(
                "Could not find specified file in role: {}/{}",
                look.sub,
                look.from
            );
        }
        return Ok(None);
    }
    let inside = folded(&base);
    for (shown, real) in &found {
        if !real.starts_with(&inside) {
            bail!(
                "Failed loading '{}' for role ({}) as it is not inside the expected role path: '{}'",
                absolute(shown).display(),
                look.name,
                absolute(&base).display()
            );
        }
    }
    Ok(Some(found.into_iter().map(|(_, real)| real).collect()))
}

/// The files of a directory read as a vars directory: in name order, hidden files and backups
/// (`~`) skipped, a sub-directory without a dot in its name read in place.
fn directory_files(
    shown: &Path,
    real: &Path,
    extensions: &[&str],
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> anyhow::Result<()> {
    let mut names: Vec<String> = std::fs::read_dir(real)
        .with_context(|| format!("reading {}", real.display()))?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.') && !n.ends_with('~'))
        .collect();
    names.sort();
    for name in names {
        let (shown, path) = (shown.join(&name), real.join(&name));
        let extension = name.rfind('.').map_or("", |at| &name[at..]);
        if extension.is_empty() && path.is_dir() {
            directory_files(&shown, &path, extensions, out)?;
        } else if path.is_file() && (extension.is_empty() || extensions.contains(&extension)) {
            out.push((shown, path));
        }
    }
    Ok(())
}

/// A path made absolute without touching the file system: no symlink is followed and `..` stays.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// An absolute path with every `.` and `..` folded out of its text, as `os.path.normpath` does.
pub(crate) fn folded(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in absolute(path).components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A role's `defaults` or `vars`, with the files of a directory merged in name order.
fn variables(role: &Path, name: &str, sub: &str, from: &str) -> anyhow::Result<Map<String, Value>> {
    let look = Lookup::named(role, name, sub, from, true);
    let mut out = Map::new();
    for file in files(&look)?.unwrap_or_default() {
        for (key, value) in crate::vars::load_vars_file(&file)? {
            out.insert(key, value);
        }
    }
    Ok(out)
}

/// The files of `meta/<from>`, which no entry names and so no entry can steer. `required` puts
/// the name with no extension first, as the reference does when it passes `main`.
fn meta_files(role: &Path, from: &str, required: bool) -> anyhow::Result<Option<Vec<PathBuf>>> {
    let name = role
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    files(&Lookup {
        required,
        ..Lookup::named(role, &name, "meta", from, false)
    })
}

/// `meta/main.yml`: the roles this one depends on, whether two identical entries run twice, and
/// the collections its tasks search.
pub(crate) fn meta(role: &Path) -> anyhow::Result<(Vec<RoleEntry>, bool, Vec<String>)> {
    let Some(files) = meta_files(role, "main", false)? else {
        return Ok((Vec::new(), false, Vec::new()));
    };
    let mut dependencies = Vec::new();
    let mut allow_duplicates = false;
    let mut collections = Vec::new();
    for file in files {
        let text = crate::vault::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let source = file.display().to_string();
        let docs = crate::yaml::load(&text, &source)?;
        let Some(doc) = docs.first() else {
            continue;
        };
        if let Some(node) = crate::yaml::field(doc, "dependencies") {
            dependencies.extend(
                crate::playbook::parse_role_entries(node)
                    .with_context(|| format!("{source}: 'dependencies'"))?,
            );
        }
        if let Some(node) = crate::yaml::field(doc, "allow_duplicates")
            && let Some(flag) = crate::yaml::as_bool(node)
        {
            allow_duplicates = flag;
        }
        if let Some(node) = crate::yaml::field(doc, "collections") {
            let bad = || anyhow::anyhow!("{source}: 'collections' must be a list of names");
            collections = match node {
                saphyr::Yaml::Value(saphyr::Scalar::Null) => Vec::new(),
                saphyr::Yaml::Sequence(items) => items
                    .iter()
                    .map(|item| item.as_str().map(str::to_string).ok_or_else(bad))
                    .collect::<anyhow::Result<_>>()?,
                other => vec![other.as_str().map(str::to_string).ok_or_else(bad)?],
            };
        }
    }
    Ok((dependencies, allow_duplicates, collections))
}

/// `meta/argument_specs.yml`, reduced to the `options` mapping of the entry named after the
/// tasks file being run. Measured: a role read with `tasks_from: extra` is checked against the
/// `extra` entry, and the banner names the entry (`Validating arguments against arg spec
/// 'main'`).
type ArgumentSpec = (Option<Map<String, Value>>, Option<String>);

fn argument_spec(role: &Path, entry: &str) -> anyhow::Result<ArgumentSpec> {
    // `_load_argument_specs` of ansible-core 2.19.12 reads the file only when one of
    // `argument_specs.yml`, `.yaml` or `.json` exists, then looks it up by name, so a bare
    // `argument_specs` wins beside one of them and is never read alone.
    let base = role.join("meta");
    if ![".yml", ".yaml", ".json"]
        .iter()
        .any(|ext| base.join(format!("argument_specs{ext}")).exists())
    {
        return Ok((None, None));
    }
    let Some(files) = meta_files(role, "argument_specs", true)? else {
        return Ok((None, None));
    };
    for file in files {
        let document = crate::vars::load_vars_file(&file)?;
        let Some(Value::Object(specs)) = document.get("argument_specs") else {
            continue;
        };
        let Some(Value::Object(spec)) = specs.get(entry) else {
            continue;
        };
        let options = match spec.get("options") {
            Some(Value::Object(options)) => options.clone(),
            _ => Map::new(),
        };
        let description = spec
            .get("short_description")
            .and_then(Value::as_str)
            .map(str::to_string);
        return Ok((Some(options), description));
    }
    Ok((None, None))
}

#[cfg(test)]
mod tests;
