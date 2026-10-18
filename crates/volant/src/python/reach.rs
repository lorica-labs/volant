// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use serde_json::Value;
use volant_protocol::modules::short_name;

use super::types::{is_resolved_name, payload_key};

/// Whether this module runs through the warm Python path.
///
/// A module ansible-core ships that this release runs neither on the agent nor on the controller,
/// and that the reference does not run through an action plugin. The first two say there is no
/// native path for it; the third says sending the module alone would run something that is not
/// what the playbook asked for, which is why those names stay refused rather than joining
/// this set.
///
/// **One definition, read by the pre-flight as well as by the driver.** The pre-flight arm that
/// lets a module reach a host calls this, so a name added or removed here cannot be admitted
/// before the first connection and then refused per host - a startup refusal silently turned
/// into a failure on every host by a change that looks local.
///
/// The five modules whose only product is facts - `setup`, `getent`, `package_facts`,
/// `service_facts` and `mount_facts` - were held out of this set while nothing merged a result's
/// `ansible_facts` into the variable store, because they would have reported `ok` and left the
/// next task reading `ansible_facts.*` undefined. `record_facts` merges them now, so they are in
/// it.
///
/// A collection's module is in the set as well: the controller's ansible-core resolves it before
/// the first connection (`union_for`), and builds it into the union like a builtin when it is
/// a module it can build.
pub fn is_python_module(module: &str) -> bool {
    use volant_protocol::modules::{
        import_module, include_module, is_builtin, is_known, short_name,
    };

    if is_resolved_name(module) {
        return true;
    }
    is_builtin(module)
        && !is_known(module)
        // The statements and the pseudo-module this engine answers itself. They are builtin
        // module names and nothing ansible-core would build a payload for: an `include_tasks`
        // sent to a host as a payload would ask the agent to run the statement the compiler
        // exists to resolve, and `meta` asks the engine for something rather than the host.
        && import_module(module).is_none()
        && include_module(module).is_none()
        && short_name(module) != crate::playbook::META
        // A plugin this release runs is not a module either: it picks one, and the sub-tasks it
        // asks for carry the payloads. `package` is gone from the refused list, so without this
        // line it would read here as a module of its own.
        && crate::action_plugins::kind(module).is_none()
}

/// The short names of every module a run needs in its union, from the modules its steps and
/// handlers name: a Python module for itself, a plugin for every module it may run.
///
/// Every backend a plugin may pick goes in before the first connection, because nothing is built
/// once the facts are known. A plugin's own name never goes in: the reference never runs the
/// `package` module, its plugin is where the choice lives.
pub fn modules_to_build<'a>(
    modules: impl IntoIterator<Item = &'a str>,
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for module in modules {
        if let Some(kind) = crate::action_plugins::kind(module) {
            out.extend(
                crate::action_plugins::modules_for(kind)
                    .iter()
                    .map(|m| (*m).to_string()),
            );
        } else if is_python_module(module) {
            out.insert(payload_key(module).to_string());
        }
    }
    out
}

/// Every module a run's union needs: the ones its compiled plays name, and the ones written in
/// every file a dynamic include may read once the run is under way.
///
/// The union is built once, before the first connection, and an include is read only when a host
/// reaches it - often under a name like `setup-{{ ansible_os_family }}.yml` that nothing can
/// resolve before the facts are in. So what is in reach goes in: every `.yml` and `.yaml` file
/// under `tasks/` and `handlers/` of each role the plays use or an include names, and of its
/// `meta/main.yml` dependencies, and every file an include or an
/// import names by a literal path, followed down. A name that is not a module this release builds
/// is dropped by [`modules_to_build`]. A collection's module goes in, and `union_for` leaves it
/// out again if the controller's ansible-core cannot build it: a file for another platform naming
/// a collection nobody installed costs nothing, and the include refuses it by name if a host ever
/// reaches it.
///
/// A role file that cannot be read or parsed refuses the run, naming the file: the role is broken.
/// A literal include target that cannot be is left to the include, which fails the host reaching
/// it the way the reference does.
pub(crate) fn modules_for_run(reach: &Reach) -> std::collections::BTreeSet<String> {
    modules_to_build(reach.modules.iter().map(|(module, _)| module.as_str()))
}

/// Every role and task file a run's plays can reach, read once before the first connection: what
/// [`modules_for_run`] builds the union from, and what `facts_read` reads the facts a run uses
/// from. It keeps what it has already read, so a ring of includes ends.
#[derive(Default)]
pub(crate) struct Reach {
    /// The root of every role reached.
    pub(crate) roles: std::collections::BTreeSet<std::path::PathBuf>,
    /// Every directory whose plugin directories join ansible-playbook's loaders, in the order it
    /// adds them (`add_all_plugin_dirs`), which is the order they are searched in.
    pub(crate) plugin_dirs: Vec<std::path::PathBuf>,
    /// Every task file read.
    pub(crate) files: std::collections::BTreeSet<std::path::PathBuf>,
    /// Every module a task names, with how many of `plugin_dirs` the reference had added when
    /// that task runs.
    modules: Vec<(String, usize)>,
    /// Every task read from a file, with that file and where it resolves its relative paths.
    pub(crate) tasks: Vec<(
        std::path::PathBuf,
        crate::compile::Origin,
        crate::playbook::PlayTask,
    )>,
}

/// One playbook the operator named: the directory it is in, and its plays, an imported file's
/// spliced in, each with the directory of the file it was written in.
pub(crate) struct Named<'a> {
    pub(crate) dir: Option<&'a std::path::Path>,
    pub(crate) plays: Vec<(&'a std::path::Path, &'a crate::compile::Compiled)>,
}

impl Reach {
    /// [`Reach::walk_run`] over plays of one playbook with no directory of its own.
    #[cfg(test)]
    pub(crate) fn walk(
        plays: &[(&std::path::Path, &crate::compile::Compiled)],
    ) -> anyhow::Result<Reach> {
        Reach::walk_run(&[Named {
            dir: None,
            plays: plays.to_vec(),
        }])
    }

    /// Walks from every step and handler of each playbook's plays, each with the directory of
    /// its play, which is where a play handler's include resolves.
    ///
    /// The plugin directories are added in the reference's order, measured on ansible-core
    /// 2.19.12 with a `library/ping.py` placed in each position: every named playbook's
    /// directory before anything runs (`cli/playbook.py`); then, as each playbook is read, just
    /// before its plays run, each imported file's directory and each play's roles, a role after
    /// its dependencies (`Playbook._load_playbook_data`, `Role._load_role_data`); and a role an
    /// `include_role` brings in when that task runs. A later playbook's roles are therefore not
    /// searched for an earlier playbook's tasks, and a later playbook's own directory is.
    pub(crate) fn walk_run(playbooks: &[Named]) -> anyhow::Result<Reach> {
        let mut reach = Reach::default();
        for playbook in playbooks {
            if let Some(dir) = playbook.dir {
                reach.add_dir(dir);
            }
        }
        for playbook in playbooks {
            for &(dir, play) in &playbook.plays {
                reach.add_dir(dir);
                for role in &play.roles {
                    if let Ok(path) = play.search.locate(&role.name) {
                        reach.add_dir(&path);
                    }
                }
            }
            for &(dir, play) in &playbook.plays {
                reach.play(dir, play)?;
            }
        }
        Ok(reach)
    }

    /// `dir`, by its real path, at the end of [`Reach::plugin_dirs`] unless it is there already.
    /// A playbook or a role inside a collection adds nothing, as in the reference.
    fn add_dir(&mut self, dir: &std::path::Path) {
        let Ok(dir) = std::fs::canonicalize(dir) else {
            return;
        };
        if !dir
            .components()
            .any(|c| c.as_os_str() == "ansible_collections")
            && !self.plugin_dirs.contains(&dir)
        {
            self.plugin_dirs.push(dir);
        }
    }

    /// Records `module` as named by a task that runs now.
    fn named(&mut self, module: &str) {
        self.modules
            .push((module.to_string(), self.plugin_dirs.len()));
    }

    /// Walks one play's steps and handlers, each task named at the time it runs.
    fn play(
        &mut self,
        dir: &std::path::Path,
        play: &crate::compile::Compiled,
    ) -> anyhow::Result<()> {
        for step in &play.steps {
            self.named(&step.task.module);
            if let Some(role) = &step.origin.role_dir {
                self.role(role, &play.search)?;
            }
            let origin = &step.origin;
            self.statement(
                &step.task,
                &origin.file_dir,
                origin.role_dir.as_deref(),
                &play.search,
            )?;
        }
        for handler in &play.handlers {
            self.named(&handler.task.module);
            let role_dir = handler
                .role
                .and_then(|i| play.roles.get(i))
                .and_then(|role| play.search.locate(&role.name).ok());
            if let Some(role) = &role_dir {
                self.role(role, &play.search)?;
            }
            let file_dir = role_dir
                .as_ref()
                .map_or_else(|| dir.to_path_buf(), |role| role.join("handlers"));
            self.statement(&handler.task, &file_dir, role_dir.as_deref(), &play.search)?;
        }
        Ok(())
    }

    fn role(
        &mut self,
        dir: &std::path::Path,
        search: &crate::roles::RoleSearch,
    ) -> anyhow::Result<()> {
        if !self.roles.insert(dir.to_path_buf()) {
            return Ok(());
        }
        // Its `meta/main.yml` dependencies run with it, wherever it was reached from.
        let (dependencies, _, collections) = crate::roles::meta(dir)?;
        for dependency in dependencies {
            if let Ok(found) = search.locate(&dependency.name) {
                self.role(&found, search)?;
            }
        }
        self.add_dir(dir);
        for sub in ["tasks", "handlers"] {
            for path in yaml_files(&dir.join(sub))? {
                self.files.insert(path.clone());
                let mut tasks = if sub == "tasks" {
                    let mut tasks = Vec::new();
                    flatten(&crate::playbook::parse_tasks_file(&path)?, &mut tasks);
                    tasks
                } else {
                    crate::playbook::parse_handlers_file(&path)?
                        .into_iter()
                        .map(|h| h.task)
                        .collect()
                };
                // A role searches the collections its own `meta/main.yml` lists, and nobody's else.
                crate::collection::scope(&mut tasks, Some(&collections), search);
                self.tasks(
                    &tasks,
                    &path,
                    path.parent().unwrap_or(dir),
                    Some(dir),
                    search,
                )?;
            }
        }
        Ok(())
    }

    fn tasks(
        &mut self,
        tasks: &[crate::playbook::PlayTask],
        file: &std::path::Path,
        file_dir: &std::path::Path,
        role_dir: Option<&std::path::Path>,
        search: &crate::roles::RoleSearch,
    ) -> anyhow::Result<()> {
        for task in tasks {
            if !task.module.is_empty() {
                self.named(&task.module);
            }
            self.statement(task, file_dir, role_dir, search)?;
            let origin = crate::compile::Origin {
                file_dir: file_dir.to_path_buf(),
                role_dir: role_dir.map(std::path::Path::to_path_buf),
                ..crate::compile::Origin::default()
            };
            self.tasks.push((file.to_path_buf(), origin, task.clone()));
        }
        Ok(())
    }

    /// Follows what an include or an import names by a literal path. A templated name is read as
    /// it is written, names no file and no role, and is left to the include.
    fn statement(
        &mut self,
        task: &crate::playbook::PlayTask,
        file_dir: &std::path::Path,
        role_dir: Option<&std::path::Path>,
        search: &crate::roles::RoleSearch,
    ) -> anyhow::Result<()> {
        let literal = |key: &str| task.args.get(key).and_then(Value::as_str);
        match short_name(&task.module) {
            "include_tasks" | "import_tasks" => {
                let Some(name) = literal("file").or_else(|| literal("_raw_params")) else {
                    return Ok(());
                };
                let path = crate::compile::beside_or_in_role(file_dir, role_dir, name);
                if !self.files.insert(path.clone()) {
                    return Ok(());
                }
                let Ok(items) = crate::playbook::parse_tasks_file(&path) else {
                    return Ok(());
                };
                let mut tasks = Vec::new();
                flatten(&items, &mut tasks);
                // What the statement searches, its play's or its role's, is what the file does.
                crate::collection::scope(&mut tasks, task.collections.as_deref(), search);
                self.tasks(
                    &tasks,
                    &path,
                    path.parent().unwrap_or(file_dir),
                    role_dir,
                    search,
                )?;
            }
            "include_role" | "import_role" => {
                if let Some(Ok(dir)) = literal("name")
                    .or_else(|| literal("role"))
                    .map(|name| search.locate(name))
                {
                    self.role(&dir, search)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

static MODULE_PATH: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Sets the directories of `-M`/`--module-path`, which hold modules directly and lead the
/// configured module path, so a playbook's or role's `library/` is still searched before them.
pub fn set_module_path(dirs: Vec<std::path::PathBuf>) {
    *MODULE_PATH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = dirs;
}

/// The directories [`set_module_path`] set.
pub fn module_path() -> Vec<std::path::PathBuf> {
    MODULE_PATH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Refuses a run in which one module name finds different files at different points of the
/// run. The union holds one module per name, built with every directory of
/// [`Reach::plugin_dirs`] searched; the reference searches only the directories it has added
/// when the task runs. Measured on ansible-core 2.19.12: with a second named playbook whose role
/// ships `library/ping.py`, the first playbook's `ping` answers `pong` and the second one's the
/// role's; after an `include_role` of a role shipping it, the `ping` before answers `pong`.
///
/// A name is safe when the first directory whose `library/` holds it was added before the
/// earliest task naming it, or when none holds it: every task then finds the same file. A name
/// a plugin's sub-tasks use (`stat` for `copy`) counts as named by the plugin's task.
pub(crate) fn check_plugin_order(reach: &Reach) -> anyhow::Result<()> {
    let mut earliest: BTreeMap<String, usize> = BTreeMap::new();
    for (module, at) in &reach.modules {
        for name in modules_to_build([module.as_str()]) {
            let name = name.strip_prefix("ansible.legacy.").unwrap_or(&name);
            if name.contains('.') {
                continue;
            }
            let first = earliest.entry(name.to_string()).or_insert(*at);
            *first = (*first).min(*at);
        }
    }
    for (name, at) in earliest {
        let Some(first) = reach.plugin_dirs.iter().position(|dir| holds(dir, &name)) else {
            continue;
        };
        if first >= at {
            bail!(
                "module '{name}' is found in {} only once ansible-playbook has added that directory, after a task naming '{name}' has run with another module under that name. Volant builds one module per name for the whole run and cannot run both. Rename the module in that directory, or run the playbooks apart.",
                reach.plugin_dirs[first].join("library").display()
            );
        }
    }
    Ok(())
}

/// Whether `dir`'s `library/` holds the module `name` as the reference's legacy search finds
/// one built with `mod_type=".py"`: a file `<name>.py`, a link to one included.
fn holds(dir: &std::path::Path, name: &str) -> bool {
    dir.join("library").join(format!("{name}.py")).is_file()
}

/// Every task of a list, blocks and all three of their sections included. A block's own keywords
/// (`when`, `vars`, `environment`, ...) come first, as a task that names no module: what they
/// read is read before any task of the block runs.
fn flatten(items: &[crate::playbook::TaskOrBlock], out: &mut Vec<crate::playbook::PlayTask>) {
    for item in items {
        match item {
            crate::playbook::TaskOrBlock::Task(task) => out.push(task.clone()),
            crate::playbook::TaskOrBlock::Block(b) => {
                out.push(b.keywords.clone());
                for section in [&b.body, &b.rescue, &b.always] {
                    flatten(section, out);
                }
            }
        }
    }
}

/// The `.yml` and `.yaml` files under `dir`, at any depth, in a stable order. A directory that is
/// not there has none.
fn yaml_files(dir: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("reading {}", dir.display()))?;
    entries.sort();
    for path in entries {
        if path.is_dir() {
            out.extend(yaml_files(&path)?);
        } else if path
            .extension()
            .is_some_and(|ext| ext == "yml" || ext == "yaml")
        {
            out.push(path);
        }
    }
    Ok(out)
}
