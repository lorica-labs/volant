// SPDX-License-Identifier: GPL-3.0-or-later
//! The controller's Python helper: one interpreter, alive for the whole run, that builds the
//! module payloads a Python task needs.
//!
//! ansible-core caches a module's zip by the module's name, so the first build of a module
//! costs 140 ms and every later one 2.8 ms - measured on ansible-core 2.19.12. A helper started
//! per task would pay the cold price every time, which is why this one is started once and
//! kept.
//!
//! What it hands back is **one** union zip for every module the run names, not one zip per
//! module: the entries the per-module zips share are byte-identical, so merging them is well
//! defined, and the union of the five modules this release ships is 631 KB against the 2.3 MB
//! the five separate zips add up to. The helper refuses a merge conflict rather than letting
//! the last module written win, because a blob whose `module_utils` came from an arbitrary
//! module would run something the playbook did not ask for.

use std::collections::BTreeMap;
use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, bail};
use serde_json::{Map, Value};
use volant_protocol::frame::{read_frame, write_frame};
use volant_protocol::modules::short_name;

/// The helper itself, shipped in the binary and handed to the interpreter on its command line.
/// Nothing is written to disk for it, so no temporary file can be left behind by a run that
/// dies.
pub(crate) const HELPER: &str = include_str!("python_helper.py");

/// The per-module facts a task needs alongside the shared blob, as the wrapper ansible-core
/// built for that module reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleFacts {
    /// `ansible.modules.ping`, the name the module is imported under.
    pub module_fqn: String,
    /// `legacy` on ansible-core 2.19.12; the serialisation profile the module was built for.
    pub profile: String,
    /// `rlimit_nofile` from the wrapper; 0 means leave the limit alone.
    pub rlimit_nofile: u64,
    /// The wrapper's `extensions`; empty on ansible-core 2.19.12.
    pub extensions: Map<String, Value>,
    /// Whether the file built is ansible-core's own module. Measured on ansible-core 2.19.12, a
    /// `library/stat.py` is built as `ansible.legacy.stat` and a collection's module under
    /// `ansible_collections.`, neither of which the agent's `ansible.modules.<name>` lookup
    /// matches; this is the second barrier, for whatever a wrapper names a module next. A helper
    /// that says nothing is read as `false`, which sends the payload.
    pub core: bool,
}

/// Where `setup` gets its facts: `--facts`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Facts {
    /// The native collector when the plays provably read only the keys it produces
    /// (`facts_read`), the Python module otherwise. Resolved to one of the two others before
    /// the union is armed; read as `Python` wherever it is not.
    #[default]
    Auto,
    Python,
    Native,
}

/// Which tasks of a run may be answered by an agent's native module instead of their payload.
///
/// The default is natives off: a union whose policy nobody set sends every task to Python, so a
/// lost assignment shows as no native ever answering rather than as natives answering a run
/// that switched them off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Natives {
    /// `[volant] native_modules`, or `VOLANT_NATIVE_MODULES`.
    pub enabled: bool,
    pub facts: Facts,
}

impl Natives {
    /// Whether a task whose module was built as `facts` has to run its payload whatever native
    /// the agent has enabled.
    ///
    /// `setup` is recognised by the name it was built under, which is the name the agent looks a
    /// native up by: a collection's module redirected to `ansible.builtin.setup` is ansible-core's
    /// `setup` whatever the task called it.
    pub fn force_python(&self, facts: &ModuleFacts) -> bool {
        if !self.enabled || !facts.core {
            return true;
        }
        facts.module_fqn == "ansible.modules.setup" && self.facts != Facts::Native
    }
}

/// One union zip and the facts of every module merged into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Union {
    /// Lowercase hex blake3 of the **decoded** zip, which is what names the blob on the wire
    /// and on the host. The agent hashes the bytes it decoded and refuses a blob whose name
    /// they do not match, so hashing anything else here - the base64 text is the easy mistake,
    /// being the same length of hex - leaves the two sides unable to agree on what a blob is
    /// called, and every run re-uploads a payload the host already holds.
    pub hash: String,
    /// The zip, base64 exactly as ansible-core produced its parts: the wrapper carries
    /// `zip_data` already encoded, so re-encoding raw bytes for the frame would decode and
    /// encode the same 631 KB for nothing.
    pub zip_b64: String,
    /// Keyed by [`payload_key`]: `ping` and `ansible.legacy.ping` are one entry, and
    /// `ansible.posix.sysctl` is its own, whatever else in the run is called `sysctl`.
    pub modules: BTreeMap<String, ModuleFacts>,
    /// A collection's module a role file named and the controller's ansible-core did not resolve
    /// to a module it can build, with the refusal the pre-flight gives a play naming it
    /// (`preflight::refusal_of`): kept out of the union, and said by the include that
    /// reaches it. A union may hold only these, with no zip and no module: nothing is sent then,
    /// since no task has a payload.
    pub refused: BTreeMap<String, String>,
    /// What the run's configuration lets a native module answer for, read into every payload.
    pub natives: Natives,
}

impl Union {
    /// The module's half of the payload a task naming `module` runs with, when the run built one.
    pub fn payload(&self, module: &str) -> Option<ModulePayload> {
        let facts = self.modules.get(payload_key(module))?;
        Some(ModulePayload {
            blob: self.hash.clone(),
            facts: facts.clone(),
            force_python: self.natives.force_python(facts),
        })
    }
}

/// The key a module's facts are filed under in [`Union::modules`], and looked up by.
///
/// A builtin written bare or as `ansible.legacy.` keeps its short name, so those two spellings of
/// `ping` are one entry: both search a playbook's or a role's `library/` first. `ansible.builtin.`
/// is kept whole, because ansible-core looks that name up among its own modules alone
/// (`_find_fq_plugin`): measured on 2.19.12 with a `library/ping.py`, `ping` and
/// `ansible.legacy.ping` run it and `ansible.builtin.ping` runs ansible-core's. Every other name
/// is kept whole too: two collections may each ship a `sysctl`, and a collection may ship a module
/// named like a builtin, so a key shortened to the last segment would hand a task the module of
/// whichever of them was built last.
pub fn payload_key(module: &str) -> &str {
    if volant_protocol::modules::is_builtin(module) && !module.starts_with("ansible.builtin.") {
        short_name(module)
    } else {
        module
    }
}

/// Whether only the controller's ansible-core can say what this name is: a collection's module,
/// or a name no builtin has (`my_module`, `ansible.legacy.my_module`), which a `library/` beside
/// the playbook, in a role, or on the configured path may hold. `ansible.builtin.nosuch` is not
/// one: it is a typo in a namespace ansible-core owns whole, and nothing can supply it.
pub fn is_resolved_name(module: &str) -> bool {
    !module.is_empty()
        && !module.starts_with("ansible.builtin.")
        && !volant_protocol::modules::is_builtin(module)
}

/// Tells the action plugin dispatch which of the names ansible-core resolved are collection
/// action plugins, which run in the helper rather than as a module.
pub(crate) fn note_action_plugins(resolved: &BTreeMap<String, Resolved>) {
    for (name, answer) in resolved {
        if let Resolved::ActionPlugin { fqcn, with_module } = answer
            && !crate::action_plugins::waits_for_later(name)
            && !crate::action_plugins::waits_for_later(fqcn)
        {
            crate::action_plugins::note_collection_action(name, fqcn, *with_module);
        }
    }
}

/// What the controller's ansible-core makes of one module name it was asked about.
///
/// Only the helper can answer: the collections are the controller's, installed where its
/// ansible-core looks for them, and nothing is fetched to find out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A module it can build; `collection` is `None` for a builtin, else `(name, version)`, the
    /// version being `*` for a collection that carries no `MANIFEST.json`, as ansible-galaxy
    /// lists it.
    Module {
        fqcn: String,
        collection: Option<(String, String)>,
    },
    /// A name a collection serves through an action plugin, its own or one its `runtime.yml`
    /// routes it to: run in the helper as ansible-core's action plugin. `fqcn` is the plugin
    /// that runs (`runtime.yml`'s `action_plugin`, else the one of the module's name), and
    /// `with_module` says the module of the asked name builds, for a plugin that runs it.
    ActionPlugin { fqcn: String, with_module: bool },
    /// A name ansible-core knows and this release cannot run: a module its collection removed
    /// (a `runtime.yml` tombstone), one with no Python source (a `.ps1` module whose `.py` holds
    /// only its documentation), or one the helper failed to build. `reason` is ansible-core's
    /// own sentence.
    Unusable { reason: String },
    /// Nothing answers to it; `collection` names what would have to be installed, and is `None`
    /// when that collection is installed and has no such module, or the name names no
    /// collection at all.
    Missing { collection: Option<String> },
}

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
/// the first connection ([`union_for`]), and builds it into the union like a builtin when it is
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
/// is dropped by [`modules_to_build`]. A collection's module goes in, and [`union_for`] leaves it
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

/// One union blob for a whole run, or `None` when no task of it needs one.
///
/// Built once, before the first connection, under a helper that lives no longer than the build:
/// ansible-core caches a module's zip by name inside one process, which is what makes building
/// five modules in one call cost one cold build and four warm ones.
///
/// An error here is a refusal the operator reads instead of a run that starts, connects, and
/// then cannot run its first Python task.
///
/// A collection's module is resolved first, by the same helper, and only what resolves to a
/// module it can build goes into the union. `named` is every `(task, module)` the compiled plays
/// name themselves: one of those that is missing is refused here
/// by `preflight::check_resolved`. A collection's module found only in a role file is
/// dropped instead, because a file for another platform is no reason to refuse the run.
///
/// The union is kept between runs ([`crate::union_cache`]): a run whose entry is still valid
/// starts no helper at all. `warn` is told, once, when the cache cannot be written.
///
/// `plugin_dirs` is every playbook's directory, then every role's, in the order ansible-playbook
/// adds them to its loaders: their `library/` and `module_utils/` are searched before anything
/// configured, and the helper is told about them before it resolves or builds anything.
pub fn union_for(
    modules: &std::collections::BTreeSet<String>,
    named: &[(String, String)],
    plugin_dirs: &[std::path::PathBuf],
    warn: &mut dyn FnMut(String),
) -> anyhow::Result<Option<Union>> {
    if modules.is_empty() {
        return Ok(None);
    }
    let place = crate::union_cache::Place::here(modules, &collection_names(modules), plugin_dirs);
    let start = || {
        let mut builder = PythonBuilder::start()?;
        builder.add_plugin_dirs(plugin_dirs)?;
        Ok(builder)
    };
    union_from(start, modules, named, place.as_ref(), warn)
}

/// [`union_for`] that also hands back the helper it built, still running, when it built one.
///
/// The run keeps it for the filters, tests and lookups ansible-core alone can answer
/// ([`crate::bridge::Bridge::adopt`]) instead of paying for a second ansible-core start.
pub fn union_and_helper(
    modules: &std::collections::BTreeSet<String>,
    named: &[(String, String)],
    plugin_dirs: &[std::path::PathBuf],
    warn: &mut dyn FnMut(String),
) -> anyhow::Result<(Option<Union>, Option<PythonBuilder>)> {
    if modules.is_empty() {
        return Ok((None, None));
    }
    let place = crate::union_cache::Place::here(modules, &collection_names(modules), plugin_dirs);
    let start = || {
        let mut builder = PythonBuilder::start()?;
        builder.add_plugin_dirs(plugin_dirs)?;
        Ok(builder)
    };
    let mut helper = None;
    let union = union_keeping(start, modules, named, place.as_ref(), warn, &mut helper)?;
    Ok((union, helper))
}

/// The names only the controller's ansible-core can answer to, which the helper resolves first.
fn collection_names(modules: &std::collections::BTreeSet<String>) -> Vec<String> {
    modules
        .iter()
        .filter(|m| is_resolved_name(m))
        .cloned()
        .collect()
}

/// [`union_for`] with the helper's start and the cache's place handed in, so what happens when
/// there is no helper, or a cache entry, is testable without touching the process environment.
fn union_from(
    start: impl FnOnce() -> anyhow::Result<PythonBuilder>,
    modules: &std::collections::BTreeSet<String>,
    named: &[(String, String)],
    cache: Option<&crate::union_cache::Place>,
    warn: &mut dyn FnMut(String),
) -> anyhow::Result<Option<Union>> {
    union_keeping(start, modules, named, cache, warn, &mut None)
}

/// [`union_from`] leaving the helper it started in `keep` instead of dropping it.
fn union_keeping(
    start: impl FnOnce() -> anyhow::Result<PythonBuilder>,
    modules: &std::collections::BTreeSet<String>,
    named: &[(String, String)],
    cache: Option<&crate::union_cache::Place>,
    warn: &mut dyn FnMut(String),
    keep: &mut Option<PythonBuilder>,
) -> anyhow::Result<Option<Union>> {
    let unusable = |place: &crate::union_cache::Place, err: &dyn std::fmt::Display| {
        format!(
            "the module cache at {} is not usable: {err}; payloads are rebuilt on every run",
            place.dir.display()
        )
    };
    let mut cache = cache;
    if let Some(place) = cache {
        match crate::union_cache::open(&place.dir, &place.key) {
            Ok(Some(entry)) => {
                // The tasks of this run are checked against the answers kept, as they would have
                // been against fresh ones: the key holds the module set, not which of them a
                // task names.
                note_action_plugins(&entry.resolved);
                for (task, module) in named {
                    if let Some(answer) = entry.resolved.get(module) {
                        crate::preflight::check_resolved(task, module, answer)?;
                    }
                }
                return Ok(Some(entry.union));
            }
            Ok(None) => {}
            // Said once, and nothing is written this run: what is there is not to be trusted,
            // and the operator is the one to look at it.
            Err(err) => {
                warn(unusable(place, &err));
                cache = None;
            }
        }
    }
    let mut names: Vec<String> = modules.iter().cloned().collect();
    let builder = match start() {
        Ok(builder) => keep.insert(builder),
        // Nothing the plays name needs a payload, only names a role file for another platform
        // holds: the run went without ansible-core before those names were resolved at all.
        Err(_)
            if names
                .iter()
                .all(|m| is_resolved_name(m) && !named.iter().any(|(_, n)| n == m)) =>
        {
            return Ok(None);
        }
        // A name a task gives outside `ansible.builtin` is refused with the reference's own
        // sentence first: `ansible.builtins.debug` is a typo before it is a reason to install
        // anything, and only ansible-core could have said which of the two it is.
        Err(err) => {
            if let Some((task, module)) = named.iter().find(|(_, m)| modules.contains(m)) {
                return Err(crate::preflight::unresolvable_here(task, module, &err));
            }
            return Err(no_builder(&err, &names));
        }
    };
    let asked = collection_names(modules);
    let mut refused = BTreeMap::new();
    let mut resolved = BTreeMap::new();
    if !asked.is_empty() {
        resolved = builder.resolve(&asked)?;
        note_action_plugins(&resolved);
        for (task, module) in named {
            if let Some(answer) = resolved.get(module) {
                crate::preflight::check_resolved(task, module, answer)?;
            }
        }
        for (module, answer) in &resolved {
            if let Some(why) = crate::preflight::refusal_of(module, answer) {
                refused.insert(module.clone(), why);
            }
        }
        names.retain(|m| {
            resolved.get(m).is_none_or(|r| {
                matches!(
                    r,
                    Resolved::Module { .. }
                        | Resolved::ActionPlugin {
                            with_module: true,
                            ..
                        }
                )
            })
        });
    }
    if names.is_empty() {
        return Ok((!refused.is_empty()).then(|| Union {
            hash: String::new(),
            zip_b64: String::new(),
            modules: BTreeMap::new(),
            refused,
            natives: Natives::default(),
        }));
    }
    let (mut union, traced) = builder.union(&names)?;
    union.refused = refused;
    let (Some(place), Some(traced)) = (cache, traced) else {
        return Ok(Some(union));
    };
    // Filed only under the interpreter that built it. When the first candidate has no
    // ansible-core, `find_python` built under another one, which the key does not name. And a
    // candidate that is a link to something other than Python - mise's shims are links to the
    // `mise` binary, which picks the version it runs - keeps its key whatever it runs: the
    // interpreter that answered has to be the one the key's real path names.
    if crate::union_cache::located(&builder.python).as_ref() != Some(&place.interpreter)
        || traced.interpreter != place.real
    {
        return Ok(Some(union));
    }
    let entry = crate::union_cache::Entry {
        union,
        sources: traced.sources,
        resolved,
    };
    if let Err(err) = crate::union_cache::store(&place.dir, &place.key, &entry) {
        warn(unusable(place, &err));
    }
    Ok(Some(entry.union))
}

/// The sentence a run that gathers facts and nothing else gets on top of [`refusal_for`].
///
/// `gather_facts` is on unless a play turns it off, so this refusal is the first thing a playbook
/// of native tasks alone meets on a controller with no ansible-core - and the way out it wants is
/// not the one the refusal names. An operator who has never installed ansible-core, and who never
/// asked for a Python module, should not have to work out that the play keyword is what put one
/// in the run.
const GATHERING_ONLY: &str = "This run needs ansible-core only to gather facts, so writing \
                              `gather_facts: false` on the play is the other way out";

/// The refusal an operator reads when the controller cannot build payloads, widened with the way
/// out that exists only when gathering facts is the whole reason one was wanted.
///
/// Said only when it is true: a run naming `lineinfile` needs ansible-core whatever the play says
/// about facts, and pointing that operator at `gather_facts: false` sends them to try something
/// that cannot work.
fn no_builder(err: &anyhow::Error, modules: &[String]) -> anyhow::Error {
    match modules {
        [only] if only == "setup" => anyhow::anyhow!("{err:#}. {GATHERING_ONLY}"),
        _ => anyhow::anyhow!("{err:#}"),
    }
}

/// What a task needs of a payload before it knows which host it is going to.
///
/// A payload has two halves with two different lifetimes. This is the module's: the blob that
/// carries it and the facts ansible-core reported building it, all of them identical on every
/// host. The other half is the interpreter, which is the host's own and is not known until its
/// agent has said what it has.
///
/// They are kept apart rather than kept together and filled in later, so that a payload with no
/// interpreter in it cannot exist: the wire `PythonPayload` is only ever built where the chosen
/// interpreter is in hand, and a module run under an interpreter nobody chose is the failure
/// this whole path is built to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModulePayload {
    /// The union blob that holds this module, by its hash.
    pub blob: String,
    pub facts: ModuleFacts,
    /// Sent as [`volant_protocol::Task::force_python`].
    pub force_python: bool,
}

impl ModulePayload {
    /// The payload as the agent receives it, under the interpreter chosen for its host.
    pub fn under(&self, interpreter: &str) -> volant_protocol::PythonPayload {
        volant_protocol::PythonPayload {
            blob: self.blob.clone(),
            module_fqn: self.facts.module_fqn.clone(),
            profile: self.facts.profile.clone(),
            rlimit_nofile: self.facts.rlimit_nofile,
            extensions: self.facts.extensions.clone(),
            interpreter: interpreter.to_string(),
        }
    }
}

/// The interpreters to try, in the order they are tried.
///
/// An explicit `VOLANT_PYTHON` is the only candidate when it is set: falling back from it to
/// `python3` would answer a path that does not exist, or has no ansible-core, by quietly
/// running something else.
pub(crate) fn candidates(explicit: Option<&str>, virtual_env: Option<&str>) -> Vec<String> {
    if let Some(python) = explicit {
        return vec![python.to_string()];
    }
    let mut out = Vec::new();
    if let Some(env) = virtual_env {
        out.push(format!("{env}/bin/python"));
    }
    out.push("python3".to_string());
    out
}

/// The sentence the pre-flight prints when no candidate has ansible-core, naming what was
/// tried and the one variable that changes the answer.
fn refusal_for(tried: &str) -> String {
    format!(
        "no Python interpreter with ansible-core: tried {tried}. Install ansible-core for one \
         of them, or set VOLANT_PYTHON to an interpreter that has it"
    )
}

/// A helper process and the two pipes a request travels over.
pub struct PythonBuilder {
    child: Child,
    /// `None` only once [`Drop`] has closed it.
    stdin: Option<ChildStdin>,
    /// `None` only once an [`ActionHelper`] reads it on a thread of its own.
    stdout: Option<BufReader<ChildStdout>>,
    /// The interpreter it runs under, as it was started.
    python: String,
}

impl PythonBuilder {
    /// Starts the helper under the first interpreter that has ansible-core.
    ///
    /// `Err` when none has, carrying the sentence the pre-flight prints. The probe is the
    /// import itself rather than a version string: an interpreter that cannot import `ansible`
    /// cannot build a payload, whatever it reports.
    pub fn start() -> anyhow::Result<PythonBuilder> {
        let explicit = std::env::var("VOLANT_PYTHON").ok();
        let virtual_env = std::env::var("VIRTUAL_ENV").ok();
        start_from(explicit.as_deref(), virtual_env.as_deref())
    }
    pub(crate) fn under(python: &str) -> anyhow::Result<PythonBuilder> {
        PythonBuilder::under_with(python, &[])
    }

    /// [`PythonBuilder::under`] with variables set for the helper alone, which is how a test
    /// points ansible-core at a collection it wrote without touching this process's environment.
    fn under_with(python: &str, env: &[(&str, &str)]) -> anyhow::Result<PythonBuilder> {
        PythonBuilder::spawn(python, &["-c", HELPER], env)
    }

    pub(crate) fn spawn(
        python: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> anyhow::Result<PythonBuilder> {
        let mut child = Command::new(python)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting the python helper under {python}"))?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));
        Ok(PythonBuilder {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            python: python.to_string(),
        })
    }

    /// The union blob for every module named, the facts each of them needs, and the files it was
    /// built from when the helper could name them all.
    ///
    /// An error here fails the run: a union that was not built whole is never sent, so no host
    /// can receive a blob missing the `module_utils` one of its tasks imports.
    pub fn union(
        &mut self,
        modules: &[String],
    ) -> anyhow::Result<(Union, Option<crate::union_cache::Traced>)> {
        let stdin = self.stdin.as_mut().expect("stdin is open until drop");
        exchange(
            stdin,
            self.stdout.as_mut().expect("stdout is read here"),
            modules,
        )
    }

    /// Adds each directory's plugin directories to the helper's loaders, as ansible-playbook
    /// adds a playbook's and a role's.
    pub fn add_plugin_dirs(&mut self, dirs: &[std::path::PathBuf]) -> anyhow::Result<()> {
        let dirs: Vec<&str> = dirs.iter().filter_map(|dir| dir.to_str()).collect();
        let module_path = module_path();
        let module_path: Vec<&str> = module_path.iter().filter_map(|dir| dir.to_str()).collect();
        if !dirs.is_empty() || !module_path.is_empty() {
            let stdin = self.stdin.as_mut().expect("stdin is open until drop");
            ask(
                stdin,
                self.stdout.as_mut().expect("stdout is read here"),
                &serde_json::json!({ "plugin_dirs": dirs, "module_path": module_path }),
                "add the plugin directories",
            )?;
        }
        Ok(())
    }

    /// One frame out and the next one in, for a caller that frames its own requests. `Ok(None)`
    /// when the helper closed its output before answering.
    pub(crate) fn round_trip(&mut self, request: &[u8]) -> std::io::Result<Option<Vec<u8>>> {
        let stdin = self.stdin.as_mut().expect("stdin is open until drop");
        write_frame(stdin, request)?;
        read_frame(self.stdout.as_mut().expect("stdout is read here"))
    }

    /// Ends the helper and says how it went, for the message of a call that found it gone.
    pub(crate) fn end(&mut self) -> String {
        drop(self.stdin.take());
        let _ = self.child.kill();
        self.child
            .wait()
            .map_or_else(|err| err.to_string(), |status| status.to_string())
    }

    /// What the controller's ansible-core makes of each name, one answer per name asked.
    pub fn resolve(&mut self, modules: &[String]) -> anyhow::Result<BTreeMap<String, Resolved>> {
        let stdin = self.stdin.as_mut().expect("stdin is open until drop");
        resolve_exchange(
            stdin,
            self.stdout.as_mut().expect("stdout is read here"),
            modules,
        )
    }
}

/// How long a dropped helper gets to exit on its own before it is killed.
const HELPER_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

impl Drop for PythonBuilder {
    /// Asked to stop first: closing stdin ends the helper's loop, and the interpreter's exit
    /// handlers remove the `ansible-local-*` directory ansible-core made at import. A helper
    /// still running after `HELPER_EXIT_GRACE` (wedged in a build) is killed, and the wait is
    /// what keeps it from being left as a zombie.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + HELPER_EXIT_GRACE;
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What the thread reading an [`ActionHelper`]'s output hands over: a frame, `None` once the
/// helper closed its output, or why the read failed.
pub(crate) type Frame = std::io::Result<Option<Vec<u8>>>;

/// A helper that runs action plugins. Its frames are read on a thread of their own, so the wait
/// for the next one ends at the task's deadline even when the plugin never answers.
pub(crate) struct ActionHelper {
    helper: PythonBuilder,
    frames: std::sync::mpsc::Receiver<Frame>,
}

/// Why no frame came from an [`ActionHelper`].
pub(crate) enum Silence {
    /// The deadline passed first.
    Expired,
    /// The reading thread is gone, which happens only once the helper's output has ended.
    Gone,
}

impl ActionHelper {
    pub(crate) fn new(mut helper: PythonBuilder) -> ActionHelper {
        let mut stdout = helper
            .stdout
            .take()
            .expect("a new helper's output is unread");
        let (send, frames) = std::sync::mpsc::channel();
        // The thread ends with the helper's output: at its exit, or once it is killed or dropped.
        std::thread::spawn(move || {
            loop {
                let frame = read_frame(&mut stdout);
                let last = !matches!(frame, Ok(Some(_)));
                if send.send(frame).is_err() || last {
                    return;
                }
            }
        });
        ActionHelper { helper, frames }
    }

    pub(crate) fn send(&mut self, frame: &[u8]) -> std::io::Result<()> {
        let stdin = self
            .helper
            .stdin
            .as_mut()
            .expect("stdin is open until drop");
        write_frame(stdin, frame)
    }

    /// The next frame, waiting no later than `deadline`.
    pub(crate) fn next(&self, deadline: std::time::Instant) -> Result<Frame, Silence> {
        use std::sync::mpsc::RecvTimeoutError;

        let left = deadline.saturating_duration_since(std::time::Instant::now());
        self.frames.recv_timeout(left).map_err(|err| match err {
            RecvTimeoutError::Timeout => Silence::Expired,
            RecvTimeoutError::Disconnected => Silence::Gone,
        })
    }

    /// Ends the helper and says how it went.
    pub(crate) fn end(&mut self) -> String {
        self.helper.end()
    }

    /// Whether the process still runs: one that died while idle is not handed a request.
    pub(crate) fn alive(&mut self) -> bool {
        matches!(self.helper.child.try_wait(), Ok(None))
    }

    /// Asks the helper to stop, without waiting: closing its input ends its loop.
    pub(crate) fn close_input(&mut self) {
        drop(self.helper.stdin.take());
    }
}

/// The probing, split from the environment read so the order that is walked, and the refusal
/// when nothing is left, are both testable without touching the process environment.
fn start_from(explicit: Option<&str>, virtual_env: Option<&str>) -> anyhow::Result<PythonBuilder> {
    PythonBuilder::under(&find_python(explicit, virtual_env)?)
}

/// The first candidate interpreter that has ansible-core, or the refusal naming what was tried.
fn find_python(explicit: Option<&str>, virtual_env: Option<&str>) -> anyhow::Result<String> {
    let tried = candidates(explicit, virtual_env);
    let found = tried.iter().find(|python| {
        Command::new(python)
            .args(["-c", "import ansible"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    });
    let Some(python) = found else {
        bail!(refusal_for(&tried.join(", ")));
    };
    Ok(python.clone())
}

/// One request and its answer, over anything that reads and writes.
///
/// Split out from [`PythonBuilder::union`] so the two ways an answer goes wrong - the helper
/// dying without writing, and the helper reporting a module it could not build - are testable
/// without an ansible-core on the machine running the tests.
///
/// The files the helper reports building from, and the interpreter it ran under, come back beside
/// the union, `None` when it could not name them all, which keeps the union out of the cache.
fn exchange<W: Write, R: Read>(
    to: W,
    from: R,
    modules: &[String],
) -> anyhow::Result<(Union, Option<crate::union_cache::Traced>)> {
    let answer = ask(
        to,
        from,
        &serde_json::json!({ "modules": modules }),
        "build the modules",
    )?;
    let zip_b64 = answer
        .get("zip_b64")
        .and_then(Value::as_str)
        .context("the python helper's answer carries no zip")?
        .to_string();
    if zip_b64.is_empty() {
        bail!("the python helper answered an empty blob; no module payload was built");
    }
    // Hashed here rather than where it is sent, so the name and the bytes are settled in one
    // place and by one decode.
    // The agent names the blob by what the same decoder reads, so the two cannot disagree on it.
    let zip = volant_protocol::encoding::b64_decode(&zip_b64)
        .map_err(|err| anyhow::anyhow!("the python helper's blob is not base64: {err}"))?;
    let hash = blake3::hash(&zip).to_hex().to_string();
    let mut facts = BTreeMap::new();
    let built = answer
        .get("modules")
        .and_then(Value::as_object)
        .context("the python helper's answer carries no module facts")?;
    for (name, value) in built {
        // Keyed by `payload_key` whatever the playbook wrote, so `ping` and
        // `ansible.legacy.ping` in one run are one entry, a collection's `sysctl` is never
        // another's, and `prepare` finds what was built for a task by the same function.
        facts.insert(payload_key(name).to_string(), module_facts(name, value)?);
    }
    // The answer has to cover the request. Without this the helper - a future one, or one that
    // failed halfway and answered anyway - can report success having built nothing, and every
    // host then receives a blob holding none of the modules its tasks name.
    for asked in modules {
        if !facts.contains_key(payload_key(asked)) {
            bail!("the python helper built no payload for '{asked}'");
        }
    }
    let sources = answer
        .get("sources")
        .and_then(crate::union_cache::sources_from)
        .zip(answer.get("interpreter").and_then(Value::as_str))
        .map(|(sources, interpreter)| crate::union_cache::Traced {
            interpreter: interpreter.into(),
            sources,
        });
    Ok((
        Union {
            hash,
            zip_b64,
            modules: facts,
            refused: BTreeMap::new(),
            natives: Natives::default(),
        },
        sources,
    ))
}

/// One request to the helper and its answer, refusing a dead helper, an answer that is not JSON
/// and the helper's own error, which names what it could not `doing`.
fn ask<W: Write, R: Read>(
    mut to: W,
    from: R,
    request: &Value,
    doing: &str,
) -> anyhow::Result<Value> {
    write_frame(&mut to, request.to_string().as_bytes())
        .context("sending a module request to the python helper")?;
    let Some(answer) = read_frame(from).context("reading the python helper's answer")? else {
        bail!("the python helper exited before answering; no module payload was built");
    };
    let answer: Value = serde_json::from_slice(&answer)
        .context("the python helper answered something that is not JSON")?;
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        bail!("the python helper could not {doing}: {error}");
    }
    Ok(answer)
}

/// The `resolve` request and its answer, one [`Resolved`] per name asked.
///
/// A name left unanswered, or answered in a shape this does not know, is refused rather than
/// read as missing: a module taken for missing is only dropped from the union when a role file
/// alone names it, and a helper that answered nothing would then build a run whose collection
/// modules all fail per host.
fn resolve_exchange<W: Write, R: Read>(
    to: W,
    from: R,
    modules: &[String],
) -> anyhow::Result<BTreeMap<String, Resolved>> {
    let answer = ask(
        to,
        from,
        &serde_json::json!({ "resolve": modules }),
        "resolve the modules",
    )?;
    let answers = answer
        .get("resolved")
        .and_then(Value::as_object)
        .context("the python helper's answer carries no resolution")?;
    let mut out = BTreeMap::new();
    for asked in modules {
        let value = answers
            .get(asked)
            .with_context(|| format!("the python helper did not resolve '{asked}'"))?;
        out.insert(asked.clone(), resolved_from(asked, value)?);
    }
    Ok(out)
}

/// One name's answer, as the helper gives it and the union cache keeps it.
pub(crate) fn resolved_from(asked: &str, value: &Value) -> anyhow::Result<Resolved> {
    let text =
        |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    Ok(if let Some(fqcn) = text(value, "module") {
        let collection = match value.get("collection").unwrap_or(&Value::Null) {
            Value::Null => None,
            Value::Array(pair) => match pair.as_slice() {
                [Value::String(name), Value::String(version)] => {
                    Some((name.clone(), version.clone()))
                }
                _ => bail!(
                    "the python helper named the collection of '{asked}' as {pair:?}, not as a name and a version"
                ),
            },
            other => bail!(
                "the python helper named the collection of '{asked}' as {other}, not as a name and a version"
            ),
        };
        Resolved::Module { fqcn, collection }
    } else if let Some(fqcn) = text(value, "action_plugin") {
        Resolved::ActionPlugin {
            fqcn,
            with_module: value.get("with_module") == Some(&Value::Bool(true)),
        }
    } else if let Some(reason) = text(value, "unusable") {
        Resolved::Unusable { reason }
    } else if let Some(missing) = value.get("missing") {
        Resolved::Missing {
            collection: missing.as_str().map(str::to_string),
        }
    } else {
        bail!(
            "the python helper resolved '{asked}' to something this release cannot read: {value}"
        );
    })
}

/// [`resolved_from`] the other way.
pub(crate) fn resolved_json(resolved: &Resolved) -> Value {
    match resolved {
        Resolved::Module { fqcn, collection } => serde_json::json!({
            "module": fqcn,
            "collection": collection.as_ref().map(|(name, version)| [name, version]),
        }),
        Resolved::ActionPlugin { fqcn, with_module } => {
            serde_json::json!({ "action_plugin": fqcn, "with_module": with_module })
        }
        Resolved::Unusable { reason } => serde_json::json!({ "unusable": reason }),
        Resolved::Missing { collection } => serde_json::json!({ "missing": collection }),
    }
}

pub(crate) fn module_facts(name: &str, value: &Value) -> anyhow::Result<ModuleFacts> {
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("the python helper reported no {key} for '{name}'"))
    };
    Ok(ModuleFacts {
        module_fqn: field("module_fqn")?,
        profile: field("profile")?,
        rlimit_nofile: value
            .get("rlimit_nofile")
            .and_then(Value::as_u64)
            .with_context(|| format!("the python helper reported no rlimit_nofile for '{name}'"))?,
        extensions: value
            .get("extensions")
            .and_then(Value::as_object)
            .cloned()
            .with_context(|| format!("the python helper reported no extensions for '{name}'"))?,
        core: value.get("core").and_then(Value::as_bool).unwrap_or(false),
    })
}

#[cfg(test)]
mod tests;
