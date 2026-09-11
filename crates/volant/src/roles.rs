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

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::playbook::{PlayTask, TaskOrBlock};
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
}

/// What one role directory holds.
///
/// No handlers: `notify` and the play's `handlers` are both still refused before the first
/// connection, so nothing could reach them, and a directory read into a list nobody runs is the
/// silent skip this project spends its tests on. They arrive with the handlers themselves.
#[derive(Debug, Clone)]
pub struct LoadedRole {
    pub defaults: Map<String, Value>,
    pub vars: Map<String, Value>,
    pub tasks: Vec<TaskOrBlock>,
    pub dependencies: Vec<RoleEntry>,
    pub allow_duplicates: bool,
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
        let mut paths = vec![playbook_dir.join("roles")];
        paths.extend(config.roles_path.iter().cloned());
        paths.push(playbook_dir.to_path_buf());
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

/// Reads one role directory.
pub fn load(path: &Path, from: &RoleFrom) -> anyhow::Result<LoadedRole> {
    let defaults = variables(path, "defaults", &from.defaults)?;
    let vars = variables(path, "vars", &from.vars)?;
    let tasks = match files(path, "tasks", &from.tasks, from.tasks != "main")? {
        None => Vec::new(),
        Some(list) => {
            let mut tasks = Vec::new();
            for file in list {
                tasks.extend(crate::playbook::parse_tasks_file(&file)?);
            }
            tasks
        }
    };
    let (dependencies, allow_duplicates) = meta(path)?;
    let (argument_spec, argument_spec_description) = argument_spec(path, &from.tasks)?;
    Ok(LoadedRole {
        defaults,
        vars,
        tasks,
        dependencies,
        allow_duplicates,
        argument_spec,
        argument_spec_description,
    })
}

/// The files one of a role's sub-directories contributes, or `None` when it has nothing to say.
///
/// Measured on ansible-core 2.19.12: `defaults/main.yml` and a `defaults/main/` directory next to
/// each other are not merged - the file wins and the directory is not read at all - and a
/// directory's own files are merged in name order, so `b.yml` beats `a.yml` and a name only
/// `a.yml` sets survives. The corpus this plan benchmarks against ships `defaults/main/` as a
/// directory, which is why the directory form is read at all.
///
/// `required` is set when the entry asked for a file by name: measured, `tasks_from: nosuch`
/// refuses the run with `Could not find specified file in role: tasks/nosuch`, while a role with
/// no `tasks/main.yml` at all runs its variables and nothing else.
fn files(
    role: &Path,
    sub: &str,
    from: &str,
    required: bool,
) -> anyhow::Result<Option<Vec<PathBuf>>> {
    let base = role.join(sub);
    for extension in ["yml", "yaml", "json"] {
        let file = base.join(format!("{from}.{extension}"));
        if file.is_file() {
            return Ok(Some(vec![file]));
        }
    }
    let dir = base.join(from);
    if dir.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| matches!(e, "yml" | "yaml" | "json"))
            })
            .collect();
        entries.sort();
        return Ok(Some(entries));
    }
    if required {
        // Exit 4 comes from the blanket the compiler wraps its failures in, which is the code
        // the reference gives this, measured.
        bail!("Could not find specified file in role: {sub}/{from}");
    }
    Ok(None)
}

/// A role's `defaults` or `vars`, with the files of a directory merged in name order.
fn variables(role: &Path, sub: &str, from: &str) -> anyhow::Result<Map<String, Value>> {
    let mut out = Map::new();
    for file in files(role, sub, from, from != "main")?.unwrap_or_default() {
        for (key, value) in crate::vars::load_vars_file(&file)? {
            out.insert(key, value);
        }
    }
    Ok(out)
}

/// `meta/main.yml`: the roles this one depends on, and whether two identical entries run twice.
fn meta(role: &Path) -> anyhow::Result<(Vec<RoleEntry>, bool)> {
    let Some(files) = files(role, "meta", "main", false)? else {
        return Ok((Vec::new(), false));
    };
    let mut dependencies = Vec::new();
    let mut allow_duplicates = false;
    for file in files {
        let text = std::fs::read_to_string(&file)
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
    }
    Ok((dependencies, allow_duplicates))
}

/// `meta/argument_specs.yml`, reduced to the `options` mapping of the entry named after the
/// tasks file being run. Measured: a role read with `tasks_from: extra` is checked against the
/// `extra` entry, and the banner names the entry (`Validating arguments against arg spec
/// 'main'`).
type ArgumentSpec = (Option<Map<String, Value>>, Option<String>);

fn argument_spec(role: &Path, entry: &str) -> anyhow::Result<ArgumentSpec> {
    let Some(files) = files(role, "meta", "argument_specs", false)? else {
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
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "volant-roles-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("roles/base/defaults/main")).unwrap();
        std::fs::create_dir_all(dir.join("roles/base/vars")).unwrap();
        std::fs::create_dir_all(dir.join("roles/base/tasks")).unwrap();
        std::fs::create_dir_all(dir.join("roles/base/meta")).unwrap();
        std::fs::write(
            dir.join("roles/base/defaults/main/a.yml"),
            "x: a\ny: from-first\n",
        )
        .unwrap();
        std::fs::write(dir.join("roles/base/defaults/main/b.yml"), "x: b\n").unwrap();
        std::fs::write(dir.join("roles/base/vars/main.yml"), "v: role-var\n").unwrap();
        std::fs::write(
            dir.join("roles/base/tasks/main.yml"),
            "- name: base task\n  debug: msg=hi\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("roles/base/meta/main.yml"),
            "allow_duplicates: true\ndependencies:\n  - role: other\n    p: v\n",
        )
        .unwrap();
        dir
    }

    /// The measured reading rules for a role's directories: a `defaults/main/` directory is
    /// merged in name order, and `meta/main.yml` is read for both of the things it can say.
    ///
    /// What would make this red: reading a directory in whatever order the filesystem hands the
    /// entries back, which makes the value of a variable depend on the machine; or dropping the
    /// first file of the directory, which loses every name the later ones do not set.
    #[test]
    fn a_defaults_directory_merges_its_files_in_name_order() {
        let dir = tree();
        let role = load(&dir.join("roles/base"), &RoleFrom::default()).unwrap();
        assert_eq!(role.defaults["x"], serde_json::json!("b"), "b.yml is later");
        assert_eq!(role.defaults["y"], serde_json::json!("from-first"));
        assert_eq!(role.vars["v"], serde_json::json!("role-var"));
        assert_eq!(role.tasks.len(), 1);
        assert!(role.allow_duplicates);
        assert_eq!(role.dependencies.len(), 1);
        assert_eq!(role.dependencies[0].name, "other");
        assert_eq!(role.dependencies[0].params["p"], serde_json::json!("v"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Measured on ansible-core 2.19.12: `defaults/main.yml` beside a `defaults/main/` directory
    /// wins outright, and the directory is not read - a run whose role read both would carry a
    /// variable the reference never gives it.
    #[test]
    fn a_file_beats_a_directory_of_the_same_name() {
        let dir = tree();
        std::fs::write(dir.join("roles/base/defaults/main.yml"), "x: fromfile\n").unwrap();
        let role = load(&dir.join("roles/base"), &RoleFrom::default()).unwrap();
        assert_eq!(role.defaults["x"], serde_json::json!("fromfile"));
        assert!(
            !role.defaults.contains_key("y"),
            "the directory is not read at all"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file the entry asked for by name and that is not there stops the run in the reference's
    /// own words; one it never asked for is simply absent.
    #[test]
    fn a_named_file_that_is_not_there_is_refused_and_a_missing_main_is_not() {
        let dir = tree();
        let from = RoleFrom {
            tasks: "nosuch".to_string(),
            ..RoleFrom::default()
        };
        let err = load(&dir.join("roles/base"), &from).unwrap_err();
        assert!(
            format!("{err:#}").contains("Could not find specified file in role: tasks/nosuch"),
            "{err:#}"
        );
        std::fs::create_dir_all(dir.join("roles/bare/defaults")).unwrap();
        std::fs::write(dir.join("roles/bare/defaults/main.yml"), "q: 1\n").unwrap();
        let role = load(&dir.join("roles/bare"), &RoleFrom::default()).unwrap();
        assert!(role.tasks.is_empty(), "a role may have no tasks at all");
        assert_eq!(role.defaults["q"], serde_json::json!(1));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The searched list, and the sentence a missing role is refused with, are the reference's
    /// own - and the exit code is 1, not the 4 the rest of an unusable playbook gets.
    ///
    /// What would make this red: a path dropped from the list, which sends an operator looking
    /// in the wrong directories; or the refusal losing its own code to the compiler's blanket,
    /// which changes the number every script around the run reads.
    #[test]
    fn a_missing_role_names_every_path_and_exits_1() {
        let dir = tree();
        let config = Config {
            roles_path: vec![PathBuf::from("/opt/roles")],
            ..Config::default()
        };
        let search = RoleSearch::new(&dir, &config);
        assert_eq!(
            search.locate("base").unwrap(),
            dir.join("roles/base"),
            "a role beside the playbook is found first"
        );
        let err = search.locate("nosuchrole").unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("the role 'nosuchrole' was not found in "),
            "{text}"
        );
        assert!(
            text.contains(&dir.join("roles").display().to_string()),
            "{text}"
        );
        assert!(text.contains("/opt/roles"), "{text}");
        assert!(text.contains(&dir.display().to_string()), "{text}");
        assert_eq!(crate::stats::error_code(&err), 1, "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A three-part name is looked for in the collections, beside the playbook first.
    #[test]
    fn a_collection_role_is_found_under_ansible_collections() {
        let dir = tree();
        let role = dir.join("collections/ansible_collections/acme/demo/roles/hello");
        std::fs::create_dir_all(role.join("tasks")).unwrap();
        std::fs::write(role.join("tasks/main.yml"), "- debug: msg=hi\n").unwrap();
        let search = RoleSearch::new(&dir, &Config::default());
        assert_eq!(search.locate("acme.demo.hello").unwrap(), role);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
