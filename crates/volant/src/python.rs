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
const HELPER: &str = include_str!("python_helper.py");

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
    /// Keyed by [`payload_key`]: `ping` and `ansible.builtin.ping` are one entry, and
    /// `ansible.posix.sysctl` is its own, whatever else in the run is called `sysctl`.
    pub modules: BTreeMap<String, ModuleFacts>,
}

/// The key a module's facts are filed under in [`Union::modules`], and looked up by.
///
/// A builtin keeps its short name, so the three spellings of `ping` are one entry. Every other
/// name is kept whole: two collections may each ship a `sysctl`, and a collection may ship a
/// module named like a builtin, so a key shortened to the last segment would hand a task the
/// module of whichever of them was built last.
pub fn payload_key(module: &str) -> &str {
    if volant_protocol::modules::is_builtin(module) {
        short_name(module)
    } else {
        module
    }
}

/// Whether only an installed collection can answer to this name: qualified, and under neither of
/// the two prefixes that name ansible-core's own modules. `ansible.builtin.nosuch` is not one: it
/// is a typo in a namespace ansible-core owns whole, and no collection can supply it.
pub fn is_collection_name(module: &str) -> bool {
    module.contains('.')
        && !module.starts_with("ansible.builtin.")
        && !module.starts_with("ansible.legacy.")
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
    /// routes it to: refused, by name.
    ActionPlugin { fqcn: String },
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

    if is_collection_name(module) {
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
        && !crate::action_plugins::is_action_backed(module)
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
pub(crate) fn modules_for_run(
    plays: &[&crate::compile::Compiled],
) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let mut reach = Reach::default();
    for play in plays {
        for step in &play.steps {
            reach.modules.push(step.task.module.clone());
            if let Some(role) = &step.origin.role_dir {
                reach.role(role, &play.search)?;
            }
            let origin = &step.origin;
            reach.statement(
                &step.task,
                &origin.file_dir,
                origin.role_dir.as_deref(),
                &play.search,
            )?;
        }
        reach
            .modules
            .extend(play.handlers.iter().map(|h| h.task.module.clone()));
    }
    Ok(modules_to_build(reach.modules.iter().map(String::as_str)))
}

/// The walk behind [`modules_for_run`], with what it has already read so a ring of includes ends.
#[derive(Default)]
struct Reach {
    roles: std::collections::BTreeSet<std::path::PathBuf>,
    files: std::collections::BTreeSet<std::path::PathBuf>,
    modules: Vec<String>,
}

impl Reach {
    fn role(
        &mut self,
        dir: &std::path::Path,
        search: &crate::roles::RoleSearch,
    ) -> anyhow::Result<()> {
        if !self.roles.insert(dir.to_path_buf()) {
            return Ok(());
        }
        // Its `meta/main.yml` dependencies run with it, wherever it was reached from.
        for dependency in crate::roles::meta(dir)?.0 {
            if let Ok(found) = search.locate(&dependency.name) {
                self.role(&found, search)?;
            }
        }
        for sub in ["tasks", "handlers"] {
            for path in yaml_files(&dir.join(sub))? {
                self.files.insert(path.clone());
                let tasks = if sub == "tasks" {
                    let mut tasks = Vec::new();
                    flatten(&crate::playbook::parse_tasks_file(&path)?, &mut tasks);
                    tasks
                } else {
                    crate::playbook::parse_handlers_file(&path)?
                        .into_iter()
                        .map(|h| h.task)
                        .collect()
                };
                self.tasks(&tasks, path.parent().unwrap_or(dir), Some(dir), search)?;
            }
        }
        Ok(())
    }

    fn tasks(
        &mut self,
        tasks: &[crate::playbook::PlayTask],
        file_dir: &std::path::Path,
        role_dir: Option<&std::path::Path>,
        search: &crate::roles::RoleSearch,
    ) -> anyhow::Result<()> {
        for task in tasks {
            self.modules.push(task.module.clone());
            self.statement(task, file_dir, role_dir, search)?;
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
                self.tasks(&tasks, path.parent().unwrap_or(file_dir), role_dir, search)?;
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

/// Every task of a list, blocks and all three of their sections included.
fn flatten(items: &[crate::playbook::TaskOrBlock], out: &mut Vec<crate::playbook::PlayTask>) {
    for item in items {
        match item {
            crate::playbook::TaskOrBlock::Task(task) => out.push(task.clone()),
            crate::playbook::TaskOrBlock::Block(b) => {
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
/// name themselves: one of those that is missing, or served by an action plugin, is refused here
/// by `preflight::check_resolved`. A collection's module found only in a role file is
/// dropped instead, because a file for another platform is no reason to refuse the run.
pub fn union_for(
    modules: &std::collections::BTreeSet<String>,
    named: &[(String, String)],
) -> anyhow::Result<Option<Union>> {
    if modules.is_empty() {
        return Ok(None);
    }
    union_from(PythonBuilder::start, modules, named)
}

/// [`union_for`] with the helper's start handed in, so what happens when there is none is
/// testable without touching the process environment.
fn union_from(
    start: impl FnOnce() -> anyhow::Result<PythonBuilder>,
    modules: &std::collections::BTreeSet<String>,
    named: &[(String, String)],
) -> anyhow::Result<Option<Union>> {
    let mut names: Vec<String> = modules.iter().cloned().collect();
    let mut builder = match start() {
        Ok(builder) => builder,
        // Nothing the plays name needs a payload, only names a role file for another platform
        // holds: the run went without ansible-core before those names were resolved at all.
        Err(_)
            if names
                .iter()
                .all(|m| is_collection_name(m) && !named.iter().any(|(_, n)| n == m)) =>
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
    let asked: Vec<String> = names
        .iter()
        .filter(|m| is_collection_name(m))
        .cloned()
        .collect();
    if !asked.is_empty() {
        let resolved = builder.resolve(&asked)?;
        for (task, module) in named {
            if let Some(answer) = resolved.get(module) {
                crate::preflight::check_resolved(task, module, answer)?;
            }
        }
        names.retain(|m| {
            resolved
                .get(m)
                .is_none_or(|r| matches!(r, Resolved::Module { .. }))
        });
    }
    if names.is_empty() {
        return Ok(None);
    }
    builder.union(&names).map(Some)
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
fn candidates(explicit: Option<&str>, virtual_env: Option<&str>) -> Vec<String> {
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
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
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
    fn under(python: &str) -> anyhow::Result<PythonBuilder> {
        PythonBuilder::under_with(python, &[])
    }

    /// [`PythonBuilder::under`] with variables set for the helper alone, which is how a test
    /// points ansible-core at a collection it wrote without touching this process's environment.
    fn under_with(python: &str, env: &[(&str, &str)]) -> anyhow::Result<PythonBuilder> {
        let mut child = Command::new(python)
            .args(["-c", HELPER])
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
            stdin,
            stdout,
        })
    }

    /// The union blob for every module named, and the facts each of them needs.
    ///
    /// An error here fails the run: a union that was not built whole is never sent, so no host
    /// can receive a blob missing the `module_utils` one of its tasks imports.
    pub fn union(&mut self, modules: &[String]) -> anyhow::Result<Union> {
        exchange(&mut self.stdin, &mut self.stdout, modules)
    }

    /// What the controller's ansible-core makes of each name, one answer per name asked.
    pub fn resolve(&mut self, modules: &[String]) -> anyhow::Result<BTreeMap<String, Resolved>> {
        resolve_exchange(&mut self.stdin, &mut self.stdout, modules)
    }
}

impl Drop for PythonBuilder {
    /// Killed rather than asked to stop: the helper holds nothing a run needs once the last
    /// answer is in, and an interpreter wedged in a build would otherwise outlive the run that
    /// started it. The wait is what keeps it from being left as a zombie.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
fn exchange<W: Write, R: Read>(to: W, from: R, modules: &[String]) -> anyhow::Result<Union> {
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
        // `ansible.builtin.ping` in one run are one entry, a collection's `sysctl` is never
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
    Ok(Union {
        hash,
        zip_b64,
        modules: facts,
    })
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
    let text =
        |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    let mut out = BTreeMap::new();
    for asked in modules {
        let value = answers
            .get(asked)
            .with_context(|| format!("the python helper did not resolve '{asked}'"))?;
        let resolved = if let Some(fqcn) = text(value, "module") {
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
            Resolved::ActionPlugin { fqcn }
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
        };
        out.insert(asked.clone(), resolved);
    }
    Ok(out)
}

fn module_facts(name: &str, value: &Value) -> anyhow::Result<ModuleFacts> {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Two modules with one short name stay two modules: a collection's module is built under its
    /// full name and looked up by it, and a builtin keeps its short key.
    ///
    /// Measured on ansible-core 2.19.12 through this helper: asked for `ansible.posix.sysctl`, it
    /// answers under that name with `module_fqn`
    /// `ansible_collections.ansible.posix.plugins.modules.sysctl`.
    ///
    /// What would make this red: a key cut down to the last segment, which files
    /// `ansible.posix.sysctl` and `community.general.sysctl` under one `sysctl` and runs whichever
    /// was built last for both tasks; or a collection's module dropped from what the run builds,
    /// which is how it was before collections were resolved at all.
    #[test]
    fn two_modules_of_one_short_name_stay_two_in_the_union() {
        let answer = br#"{"zip_b64": "UEsD", "modules": {
            "sysctl": {"module_fqn": "ansible.modules.sysctl", "profile": "legacy",
                "rlimit_nofile": 0, "extensions": {}},
            "ansible.posix.sysctl": {"module_fqn":
                "ansible_collections.ansible.posix.plugins.modules.sysctl", "profile": "legacy",
                "rlimit_nofile": 0, "extensions": {}},
            "community.general.sysctl": {"module_fqn":
                "ansible_collections.community.general.plugins.modules.sysctl",
                "profile": "legacy", "rlimit_nofile": 0, "extensions": {}}}}"#;
        let asked =
            ["sysctl", "ansible.posix.sysctl", "community.general.sysctl"].map(str::to_string);
        let union = exchange(Vec::new(), framed(answer), &asked).unwrap();
        assert_eq!(union.modules.len(), 3, "{:?}", union.modules.keys());
        assert_eq!(
            union.modules[payload_key("ansible.posix.sysctl")].module_fqn,
            "ansible_collections.ansible.posix.plugins.modules.sysctl"
        );
        assert_eq!(
            union.modules[payload_key("community.general.sysctl")].module_fqn,
            "ansible_collections.community.general.plugins.modules.sysctl"
        );
        assert_eq!(payload_key("ansible.builtin.ping"), "ping");
        assert_eq!(payload_key("ansible.legacy.ping"), "ping");
        let built = modules_to_build([
            "ansible.posix.sysctl",
            "community.general.sysctl",
            "ansible.builtin.ping",
            "ping",
        ]);
        assert_eq!(
            built.iter().map(String::as_str).collect::<Vec<_>>(),
            ["ansible.posix.sysctl", "community.general.sysctl", "ping"]
        );
    }

    /// The helper's resolution, read one name at a time.
    ///
    /// What would make this red: a name left unanswered read as missing, which drops it from the
    /// union and fails its task on every host instead of the run on the controller; a shape
    /// this does not know read as anything at all; or a collection that is not a name and a
    /// version read as a builtin.
    #[test]
    fn the_helper_resolution_is_read_name_by_name() {
        let answer = br#"{"resolved": {
            "a.b.mod": {"module": "a.b.mod", "collection": ["a.b", "1.2.3"]},
            "a.b.act": {"action_plugin": "a.b.act"},
            "a.b.none": {"missing": null},
            "c.d.mod": {"missing": "c.d"},
            "a.b.gone": {"unusable": "The 'a.b.gone' module has been removed."}}}"#;
        let asked = ["a.b.mod", "a.b.act", "a.b.none", "c.d.mod", "a.b.gone"].map(str::to_string);
        let mut sent = Vec::new();
        let resolved = resolve_exchange(&mut sent, framed(answer), &asked).unwrap();
        assert!(
            String::from_utf8_lossy(&sent).contains(r#"{"resolve":["a.b.mod","#),
            "{sent:?}"
        );
        assert_eq!(
            resolved.into_iter().collect::<Vec<_>>(),
            [
                (
                    "a.b.act".to_string(),
                    Resolved::ActionPlugin {
                        fqcn: "a.b.act".into()
                    }
                ),
                (
                    "a.b.gone".to_string(),
                    Resolved::Unusable {
                        reason: "The 'a.b.gone' module has been removed.".into()
                    }
                ),
                (
                    "a.b.mod".to_string(),
                    Resolved::Module {
                        fqcn: "a.b.mod".into(),
                        collection: Some(("a.b".into(), "1.2.3".into())),
                    }
                ),
                (
                    "a.b.none".to_string(),
                    Resolved::Missing { collection: None }
                ),
                (
                    "c.d.mod".to_string(),
                    Resolved::Missing {
                        collection: Some("c.d".into())
                    }
                ),
            ]
        );
        for (answer, says) in [
            (&br#"{"resolved": {}}"#[..], "did not resolve 'x.y.z'"),
            (br#"{"resolved": {"x.y.z": {"other": 1}}}"#, "cannot read"),
            (
                br#"{"resolved": {"x.y.z": {"module": "x.y.z", "collection": "x.y"}}}"#,
                "not as a name and a version",
            ),
            (
                br#"{"error": "RuntimeError: boom"}"#,
                "could not resolve the modules: RuntimeError: boom",
            ),
        ] {
            let err = resolve_exchange(Vec::new(), framed(answer), &["x.y.z".to_string()])
                .unwrap_err()
                .to_string();
            assert!(err.contains(says), "{err}");
        }
    }

    /// A controller without ansible-core refuses a run whose plays name a collection's module,
    /// and runs one whose only such names sit in a role file for another platform, as it did
    /// before those names were read at all.
    ///
    /// What would make this red: the helper demanded for a name nothing will run on this host,
    /// which refuses a native playbook over a role's other-platform file; or the refusal dropped
    /// for a name a task does run, which reaches the host with no payload.
    #[test]
    fn a_role_file_alone_never_demands_ansible_core() {
        let none = || Err(anyhow::anyhow!("{}", refusal_for("python3")));
        let modules = std::collections::BTreeSet::from(["community.general.zypper".to_string()]);
        assert_eq!(union_from(none, &modules, &[]).unwrap(), None);
        let named = [("Z".to_string(), "community.general.zypper".to_string())];
        let err = union_from(none, &modules, &named).unwrap_err().to_string();
        assert!(err.contains("VOLANT_PYTHON"), "{err}");
        let modules = std::collections::BTreeSet::from([
            "community.general.zypper".to_string(),
            "ping".to_string(),
        ]);
        let err = union_from(none, &modules, &[]).unwrap_err().to_string();
        assert!(err.contains("VOLANT_PYTHON"), "{err}");

        // A dotted typo a task gives is the reference's misspelling first, then why nothing could
        // tell a typo from a missing collection. Red if it reads as the interpreter refusal
        // alone, which sends the operator to install ansible-core to fix a typo.
        let modules = std::collections::BTreeSet::from(["ansible.builtins.debug".to_string()]);
        let named = [("D".to_string(), "ansible.builtins.debug".to_string())];
        let err = union_from(none, &modules, &named).unwrap_err();
        assert_eq!(crate::stats::error_code(&err), 4, "{err:#}");
        let err = format!("{err:#}");
        assert!(
            err.starts_with(
                "task 'D': couldn't resolve module/action 'ansible.builtins.debug'. This often indicates a misspelling"
            ),
            "{err}"
        );
        assert!(err.contains("VOLANT_PYTHON"), "{err}");
    }

    /// A collection written for the test, under its own `ANSIBLE_COLLECTIONS_PATH`, holding every
    /// shape the controller has to tell apart. Measured on ansible-core 2.19.12 with this very
    /// layout: `routed` has `action_plugin` set by `runtime.yml`, `winmod` resolves to its `.ps1`
    /// without `mod_type` and to a documentation-only `.py` with it, `onlyps` resolves to nothing
    /// with it, `gone` raises `AnsiblePluginRemovedError` (as `community.general.atomic_host` does
    /// in community.general 13.4.0), and `moved` resolves to nothing with a `redirect_list` ending
    /// in `absentns.absent.moved`.
    fn fixture_collection(root: &std::path::Path) {
        let coll = root.join("ansible_collections/volanttest/coll");
        for dir in ["plugins/modules", "plugins/action", "meta"] {
            std::fs::create_dir_all(coll.join(dir)).unwrap();
        }
        let write = |path: &str, text: &str| std::fs::write(coll.join(path), text).unwrap();
        write(
            "MANIFEST.json",
            r#"{"collection_info": {"namespace": "volanttest", "name": "coll", "version": "1.0.0"}}"#,
        );
        let module = "from ansible.module_utils.basic import AnsibleModule\n\ndef main():\n    AnsibleModule(argument_spec={}).exit_json(changed=False)\n\nif __name__ == '__main__':\n    main()\n";
        write("plugins/modules/good.py", module);
        write("plugins/modules/routed.py", module);
        write("plugins/modules/winmod.py", "DOCUMENTATION = 'x'\n");
        write("plugins/modules/winmod.ps1", "#!powershell\n");
        write("plugins/modules/onlyps.ps1", "#!powershell\n");
        write(
            "plugins/action/act.py",
            "from ansible.plugins.action import ActionBase\n\nclass ActionModule(ActionBase):\n    pass\n",
        );
        write(
            "meta/runtime.yml",
            "requires_ansible: '>=2.15'\nplugin_routing:\n  modules:\n    routed:\n      action_plugin: volanttest.coll.act\n    gone:\n      tombstone:\n        removal_version: 1.0.0\n        warning_text: use good instead\n    moved:\n      redirect: absentns.absent.moved\n",
        );
    }

    /// Every shape a collection's name can take on the controller, each answered on its own, and
    /// only the buildable ones reaching the union.
    ///
    /// What would make this red: `runtime.yml`'s `action_plugin` ignored, which sends `routed`
    /// without the controller half its collection routes it to; the loader left to pick `.ps1`
    /// or the documentation `.py`, which answers `Module` for something `union` then fails to
    /// build, taking the whole run down over a Windows file of a cross-platform role; a
    /// tombstone raising out of the request, which fails the run for a name no task may run; or
    /// a redirect into a missing collection answered without that collection's name.
    #[test]
    fn a_collection_the_controller_cannot_run_is_answered_name_by_name() {
        let python = match find_python(
            std::env::var("VOLANT_PYTHON").ok().as_deref(),
            std::env::var("VIRTUAL_ENV").ok().as_deref(),
        ) {
            Ok(python) => python,
            Err(why) => return skip_or_fail(&format!("{why:#}")),
        };
        let root = std::env::temp_dir().join(format!("volant-fixture-coll-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        fixture_collection(&root);
        let path = root.display().to_string();
        let start = || PythonBuilder::under_with(&python, &[("ANSIBLE_COLLECTIONS_PATH", &path)]);
        let names = ["good", "routed", "winmod", "onlyps", "gone", "moved"]
            .map(|m| format!("volanttest.coll.{m}"));
        let resolved = start().unwrap().resolve(&names).unwrap();
        assert_eq!(
            resolved["volanttest.coll.good"],
            Resolved::Module {
                fqcn: "volanttest.coll.good".into(),
                collection: Some(("volanttest.coll".into(), "1.0.0".into())),
            }
        );
        assert_eq!(
            resolved["volanttest.coll.routed"],
            Resolved::ActionPlugin {
                fqcn: "volanttest.coll.routed".into()
            }
        );
        assert!(
            matches!(&resolved["volanttest.coll.winmod"], Resolved::Unusable { reason } if reason.contains("'old'")),
            "{:?}",
            resolved["volanttest.coll.winmod"]
        );
        assert_eq!(
            resolved["volanttest.coll.onlyps"],
            Resolved::Missing { collection: None }
        );
        assert!(
            matches!(&resolved["volanttest.coll.gone"], Resolved::Unusable { reason } if reason.contains("has been removed. use good instead")),
            "{:?}",
            resolved["volanttest.coll.gone"]
        );
        assert_eq!(
            resolved["volanttest.coll.moved"],
            Resolved::Missing {
                collection: Some("absentns.absent".into())
            }
        );

        // Found only by the role scan, the unusable names stay out and the run builds the rest.
        let modules = std::collections::BTreeSet::from(names.clone());
        let built = union_from(start, &modules, &[])
            .unwrap()
            .expect("good is built");
        assert_eq!(
            built.modules.keys().collect::<Vec<_>>(),
            ["volanttest.coll.good"]
        );
        // Named by a task, each is refused by its own sentence.
        for (module, says) in [
            (
                "volanttest.coll.winmod",
                "cannot run: module 'volanttest.coll.winmod' is built as 'old'",
            ),
            (
                "volanttest.coll.gone",
                "cannot run: AnsiblePluginRemovedError: The 'volanttest.coll.gone' module has been removed",
            ),
            (
                "volanttest.coll.moved",
                "ansible-galaxy collection install absentns.absent",
            ),
        ] {
            let named = [("T".to_string(), module.to_string())];
            let err = union_from(start, &modules, &named).unwrap_err();
            assert!(format!("{err:#}").contains(says), "{module}: {err:#}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The version of `ansible.posix` the collection tests are written against, the one
    /// installed on the controller that measured them.
    const ANSIBLE_POSIX: &str = "2.2.2";

    /// The same for `community.general`.
    const COMMUNITY_GENERAL: &str = "13.4.0";

    /// The helper under the controller's ansible-core, or `None` after saying why the test is
    /// skipped. Under `VOLANT_PYTHON` there is no skip.
    fn helper_or_skip() -> Option<PythonBuilder> {
        match PythonBuilder::start() {
            Ok(helper) => Some(helper),
            Err(why) => {
                skip_or_fail(&format!("{why:#}"));
                None
            }
        }
    }

    fn skip_or_fail(why: &str) {
        if std::env::var_os("VOLANT_PYTHON").is_some() {
            panic!("VOLANT_PYTHON cannot resolve the pinned collections: {why}");
        }
        eprintln!(
            "skipped: {why}. Set VOLANT_PYTHON to an ansible-core with ansible.posix {ANSIBLE_POSIX}."
        );
    }

    /// The controller's own ansible-core answers what its collections hold, and a collection's
    /// module lands in the union under its full name.
    ///
    /// Measured on ansible-core 2.19.12 with `ansible.posix` 2.2.2 installed under
    /// `~/.ansible/collections`: `sysctl` is a module with no action plugin, `synchronize` has one
    /// (`ansible_collections.ansible.posix.plugins.action.synchronize`), and a name in a
    /// collection nobody installed resolves to nothing.
    ///
    /// What would make this red: the collection's version read from anything but its manifest;
    /// `synchronize` answered as a module, which would send it without its controller half; a
    /// missing collection answered without its name, which leaves the operator with the typo
    /// sentence; a scan-only name that cannot be built kept in the union, which refuses the build;
    /// or a task naming `synchronize` let through.
    #[test]
    fn the_controller_resolves_what_its_collections_hold() {
        let Some(mut helper) = helper_or_skip() else {
            return;
        };
        let asked = [
            "ansible.posix.sysctl",
            "ansible.posix.synchronize",
            "ansible.posix.nosuch",
            "nosuch.coll.mod",
            "nodots.x",
            "community.general.ufw",
            "community.general.atomic_host",
        ]
        .map(str::to_string);
        let resolved = helper.resolve(&asked).unwrap();
        let sysctl = Resolved::Module {
            fqcn: "ansible.posix.sysctl".into(),
            collection: Some(("ansible.posix".into(), ANSIBLE_POSIX.into())),
        };
        if resolved["ansible.posix.sysctl"] != sysctl {
            skip_or_fail(&format!(
                "the controller's ansible.posix.sysctl is {:?}, not ansible.posix {ANSIBLE_POSIX}",
                resolved["ansible.posix.sysctl"]
            ));
            return;
        }
        assert_eq!(
            resolved["ansible.posix.synchronize"],
            Resolved::ActionPlugin {
                fqcn: "ansible.posix.synchronize".into()
            }
        );
        assert_eq!(
            resolved["ansible.posix.nosuch"],
            Resolved::Missing { collection: None }
        );
        assert_eq!(
            resolved["nosuch.coll.mod"],
            Resolved::Missing {
                collection: Some("nosuch.coll".into())
            }
        );
        assert_eq!(resolved["nodots.x"], Resolved::Missing { collection: None });
        // community.general 13.4.0's `runtime.yml` tombstones `atomic_host` (removed in 13.0.0):
        // the loader raises for it, and that is this name's answer, not the request's.
        let general = Resolved::Module {
            fqcn: "community.general.ufw".into(),
            collection: Some(("community.general".into(), COMMUNITY_GENERAL.into())),
        };
        if resolved["community.general.ufw"] == general {
            assert!(
                matches!(&resolved["community.general.atomic_host"], Resolved::Unusable { reason }
                    if reason.contains("'community.general.atomic_host' module has been removed")),
                "{:?}",
                resolved["community.general.atomic_host"]
            );
        } else {
            skip_or_fail(&format!(
                "the controller's community.general.ufw is {:?}, not community.general {COMMUNITY_GENERAL}",
                resolved["community.general.ufw"]
            ));
        }
        let union = helper
            .union(&["ansible.posix.sysctl".to_string(), "ping".to_string()])
            .unwrap();
        assert_eq!(
            union.modules["ansible.posix.sysctl"].module_fqn,
            "ansible_collections.ansible.posix.plugins.modules.sysctl"
        );
        assert_eq!(union.modules["ping"].module_fqn, "ansible.modules.ping");

        let modules = std::collections::BTreeSet::from(
            ["ansible.posix.synchronize", "nosuch.coll.mod", "ping"].map(str::to_string),
        );
        let built = union_from(PythonBuilder::start, &modules, &[])
            .unwrap()
            .expect("ping is built");
        assert_eq!(built.modules.keys().collect::<Vec<_>>(), ["ping"]);
        let named = [("Sync".to_string(), "ansible.posix.synchronize".to_string())];
        let err = union_from(PythonBuilder::start, &modules, &named).unwrap_err();
        assert_eq!(crate::stats::error_code(&err), 4, "{err:#}");
        assert!(
            format!("{err:#}")
                .contains("module 'ansible.posix.synchronize' needs an action plugin"),
            "{err:#}"
        );
    }

    /// The controller's interpreter is chosen in one order and that order is visible: an
    /// explicit `VOLANT_PYTHON`, then the active virtualenv, then `python3` on `PATH`.
    ///
    /// What would make this red: `PATH` consulted first, which would pick the system Python on
    /// a machine whose ansible-core lives in a virtualenv - measured, the dev host is exactly
    /// that machine, and its `/usr/bin/python3` has no ansible-core.
    #[test]
    fn the_controller_interpreter_follows_one_visible_order() {
        assert_eq!(candidates(Some("/opt/py"), Some("/venv"))[0], "/opt/py");
        assert_eq!(candidates(None, Some("/venv"))[0], "/venv/bin/python");
        assert_eq!(candidates(None, None), vec!["python3".to_string()]);
    }

    /// An interpreter without ansible-core is refused before the first connection, in words
    /// that say what to install and which interpreter was tried.
    #[test]
    fn an_interpreter_without_ansible_core_is_refused_before_connecting() {
        let err = refusal_for("/usr/bin/python3");
        assert!(err.contains("/usr/bin/python3"), "{err}");
        assert!(err.contains("ansible-core"), "{err}");
        assert!(err.contains("VOLANT_PYTHON"), "{err}");
    }

    /// A run that wanted ansible-core only to gather facts is told the way out that costs
    /// nothing, and a run that wanted it for a module the playbook named is not.
    ///
    /// What would make this red: the sentence left as it was. `gather_facts` is on unless a play
    /// turns it off, so this refusal is what a playbook of native tasks alone now meets on a
    /// controller with no ansible-core, and "install ansible-core" is the expensive half of the
    /// answer. Red the other way if the hint is unconditional: an operator whose playbook names
    /// `lineinfile` would be sent to write `gather_facts: false` and find the run refused all the
    /// same.
    #[test]
    fn a_run_that_only_gathers_is_told_it_can_stop_gathering() {
        let base = anyhow::anyhow!("{}", refusal_for("python3"));
        let only = format!("{:#}", no_builder(&base, &["setup".to_string()]));
        assert!(only.contains("gather_facts: false"), "{only}");
        assert!(only.contains("VOLANT_PYTHON"), "{only}");
        let also = format!(
            "{:#}",
            no_builder(&base, &["lineinfile".to_string(), "setup".to_string()])
        );
        assert!(!also.contains("gather_facts"), "{also}");
        assert!(also.contains("ansible-core"), "{also}");
    }

    /// `VOLANT_PYTHON` is the only candidate when it is set, so a path that does not exist is
    /// refused by its own name.
    ///
    /// What would make this red: appending the usual candidates behind it, which would build
    /// the payloads under a different interpreter than the operator named and say nothing.
    #[test]
    fn an_explicit_interpreter_is_never_fallen_back_from() {
        assert_eq!(
            candidates(Some("/nowhere/python"), Some("/venv")),
            vec!["/nowhere/python".to_string()]
        );
        let err = start_from(Some("/nowhere/python"), None)
            .map(|_| ())
            .expect_err("an interpreter that does not exist cannot build a payload")
            .to_string();
        assert!(err.contains("/nowhere/python"), "{err}");
        assert!(err.contains("VOLANT_PYTHON"), "{err}");
    }

    /// The probing itself walks the order and refuses when nothing is left, which is what the
    /// development machine exercises: its `python3` has no ansible-core.
    ///
    /// What would make this red: a candidate accepted because it merely ran - the probe is the
    /// `import ansible` exiting 0, not the interpreter existing - or a refusal that names
    /// something other than what was tried.
    #[test]
    fn nothing_is_started_under_an_interpreter_without_ansible_core() {
        let err = start_from(Some("/nowhere/python"), Some("/venv"))
            .map(|_| ())
            .expect_err("no candidate here has ansible-core")
            .to_string();
        assert!(
            err.contains("no Python interpreter with ansible-core"),
            "{err}"
        );
        assert!(
            !err.contains("/venv"),
            "an explicit interpreter stands alone: {err}"
        );
    }

    /// A helper that dies without writing fails the run. What would make this red: reading a
    /// closed pipe as an empty union, which would ship a blob holding no `module_utils` at all
    /// and fail every task on the host instead of the run on the controller.
    #[test]
    fn a_helper_that_dies_without_answering_fails_the_run() {
        let err = exchange(Vec::new(), Cursor::new(Vec::new()), &["ping".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("exited before answering"), "{err}");
    }

    /// What the helper could not build comes back naming the module. What would make this red:
    /// dropping the helper's own sentence, which is the only place the module's name appears.
    #[test]
    fn a_module_the_helper_cannot_build_comes_back_named() {
        let answer = br#"{"error": "RuntimeError: ansible-core has no module 'nosuch'"}"#;
        let err = exchange(Vec::new(), framed(answer), &["nosuch".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("nosuch"), "{err}");
    }

    /// The request names the modules asked for, and the facts come back keyed by module.
    #[test]
    fn a_union_carries_the_blob_and_one_set_of_facts_per_module() {
        let answer = br#"{"zip_b64": "UEsD", "modules": {"ping": {"module_fqn":
            "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0,
            "extensions": {}}}}"#;
        let mut sent = Vec::new();
        let union = exchange(&mut sent, framed(answer), &["ping".to_string()]).unwrap();
        assert!(
            String::from_utf8_lossy(&sent).contains(r#"{"modules":["ping"]}"#),
            "{sent:?}"
        );
        assert_eq!(union.zip_b64, "UEsD");
        assert_eq!(
            union.modules["ping"],
            ModuleFacts {
                module_fqn: "ansible.modules.ping".to_string(),
                profile: "legacy".to_string(),
                rlimit_nofile: 0,
                extensions: Map::new(),
            }
        );
    }

    /// Every module asked for has to come back. What would make this red: reading the answer's
    /// module map on its own, which answers five modules asked for and none built with an empty
    /// union and `Ok` - a run that then sends every host a blob holding nothing it needs.
    #[test]
    fn an_answer_that_leaves_a_module_out_is_refused() {
        let asked = ["ping", "stat", "file", "copy", "setup"].map(str::to_string);
        let answer = br#"{"zip_b64": "UEsD", "modules": {}}"#;
        let err = exchange(Vec::new(), framed(answer), &asked)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ping"), "{err}");
    }

    /// The facts are keyed by the short module name whatever spelling the playbook used, so one
    /// module asked for under two names is one entry.
    ///
    /// What would make this red: keying by the request string, which hands a map keyed
    /// `ansible.builtin.ping` to a caller that looks its task's module up by its short name and
    /// finds nothing - or, worse, an empty fact set.
    #[test]
    fn the_facts_are_keyed_by_the_short_module_name() {
        let answer = br#"{"zip_b64": "UEsD", "modules": {"ansible.builtin.ping": {"module_fqn":
            "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0,
            "extensions": {}}}}"#;
        let asked = ["ansible.builtin.ping".to_string()];
        let union = exchange(Vec::new(), framed(answer), &asked).unwrap();
        assert_eq!(union.modules.keys().collect::<Vec<_>>(), ["ping"]);
    }

    /// An empty blob is refused on the controller. What would make this red: letting it through,
    /// which moves the refusal from before the first connection to every host in the run.
    #[test]
    fn an_empty_blob_is_refused_before_it_is_sent() {
        let answer = br#"{"zip_b64": "", "modules": {}}"#;
        let err = exchange(Vec::new(), framed(answer), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty"), "{err}");
    }

    /// Which modules go through the payload path: what ansible-core ships, minus what this
    /// release runs itself, minus what the reference runs through an action plugin.
    ///
    /// What would make this red: the action-plugin names let in, which would send `package` or
    /// `template` as a module and run something the playbook did not ask for - the plugin is
    /// where their behaviour lives. Or a name this release already runs let in, which would ship
    /// a payload for `command` and stop running it natively.
    #[test]
    fn the_payload_path_takes_what_nothing_else_runs() {
        for through in ["lineinfile", "stat", "ping", "apt", "ansible.builtin.file"] {
            assert!(is_python_module(through), "{through}");
        }
        for native in [
            "command",
            "shell",
            "raw",
            "debug",
            "set_fact",
            "include_vars",
        ] {
            assert!(!is_python_module(native), "{native} runs here already");
        }
        for plugin in ["package", "service", "template", "copy", "assert"] {
            assert!(
                !is_python_module(plugin),
                "{plugin} needs its action plugin"
            );
        }
        assert!(
            is_python_module("community.general.lineinfile"),
            "a collection's module goes to the controller's ansible-core to resolve"
        );
        assert!(
            !is_python_module("ansible.builtin.nosuch"),
            "no collection can supply a name ansible-core owns"
        );
    }

    /// A run naming a plugin builds every module the plugin may pick, and never the plugin's own
    /// name.
    ///
    /// What would make this red: `package` in the set, which is `package` built and sent as
    /// itself - the reference never runs that module, its plugin is where the choice lives - or a
    /// backend missing, which leaves a host whose manager is `dnf` failing on a module the union
    /// was never asked for.
    #[test]
    fn a_plugin_brings_its_backends_into_the_union_and_not_itself() {
        let built = modules_to_build(["ansible.builtin.package", "service", "ping", "command"]);
        let built: Vec<&str> = built.iter().map(String::as_str).collect();
        assert_eq!(
            built,
            [
                "apt",
                "dnf",
                "dnf5",
                "ping",
                "service",
                "setup",
                "systemd",
                "systemd_service",
                "sysvinit"
            ]
        );
        assert!(!built.contains(&"package"), "{built:?}");
    }

    /// The union a run builds, for the first play of `text` written at `dir`.
    fn union_of(dir: &std::path::Path, text: &str) -> anyhow::Result<Vec<String>> {
        let pb = crate::playbook::parse(text, dir.join("play.yml").to_str().unwrap())?;
        let search = crate::roles::RoleSearch {
            paths: vec![dir.join("roles")],
            collections: Vec::new(),
        };
        let selection = crate::compile::TagSelection::new(Vec::new(), Vec::new());
        let play = crate::compile::compile(&pb.plays[0], &search, &selection)?;
        Ok(modules_for_run(&[&play])?.into_iter().collect())
    }

    /// A module written only in a file a dynamic include reads goes into the union, and so does
    /// one in a role file nothing includes by a name the compiler can read.
    ///
    /// Measured on ansible-core 2.19.12, `connection: local`: a play whose only task is
    /// `include_tasks: inc.yml`, the file holding one `stat`, runs the `stat` (`ok: [h1]`).
    /// `geerlingguy.security` and `geerlingguy.nginx` put their `package` and `apt` tasks behind
    /// `include_tasks: setup-{{ ansible_os_family }}.yml`, a name no compiler can resolve before
    /// the facts are in, and the union built from the compiled steps alone left them out: the
    /// run stopped at `module 'apt' needs a python payload, and this run built none for it`.
    ///
    /// What would make this red: the union built from the compiled steps alone, which leaves
    /// out `apt`, `stat` and `ping`; or a collection's module in the other platform's file left
    /// out, which is then never resolved and fails by name on a host that does reach it.
    #[test]
    fn the_union_holds_what_a_dynamic_include_may_read() {
        let dir = std::env::temp_dir().join(format!("volant-union-reach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tasks = dir.join("roles/r/tasks");
        std::fs::create_dir_all(tasks.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("roles/r/handlers")).unwrap();
        let write = |path: &str, text: &str| std::fs::write(dir.join(path), text).unwrap();
        write(
            "roles/r/tasks/main.yml",
            "- include_tasks: \"setup-{{ ansible_os_family }}.yml\"\n",
        );
        write(
            "roles/r/tasks/setup-Debian.yml",
            "- block:\n    - apt: name=x\n",
        );
        write(
            "roles/r/tasks/setup-Suse.yml",
            // Written for other platforms, and read here only for the union: a free-form Windows
            // command and a `key=value` string with a spaced template in it are parsed, not
            // refused, whatever this release would make of them if a host reached them.
            "- community.general.zypper: name=x\n- ansible.windows.win_shell: Get-Service foo\n- ns.coll.mod: port={{ x }}\n",
        );
        write(
            "roles/r/tasks/sub/deep.yaml",
            "- lineinfile: path=/x line=y\n",
        );
        write(
            "roles/r/handlers/main.yml",
            "- name: h\n  systemd: name=x\n",
        );
        write("inc.yml", "- stat: path=/\n- include_tasks: inner.yml\n");
        write("inner.yml", "- ping:\n- include_role: {name: dyn}\n");
        // A role reached only through that `include_role`, and its dependency.
        for role in ["dyn/tasks", "dyn/meta", "dep/tasks"] {
            std::fs::create_dir_all(dir.join("roles").join(role)).unwrap();
        }
        write("roles/dyn/tasks/main.yml", "- debug: msg=x\n");
        write("roles/dyn/meta/main.yml", "dependencies: [dep]\n");
        write("roles/dep/tasks/main.yml", "- apt_repository: repo=x\n");
        let union = union_of(
            &dir,
            "- hosts: all\n  gather_facts: false\n  roles: [r]\n  tasks:\n    - include_tasks: inc.yml\n",
        )
        .unwrap();
        assert_eq!(
            union,
            [
                "ansible.windows.win_shell",
                "apt",
                "apt_repository",
                "community.general.zypper",
                "lineinfile",
                "ns.coll.mod",
                "ping",
                "stat",
                "systemd"
            ]
        );

        // A role file that is not YAML is a broken role, refused by its own name.
        write("roles/r/tasks/broken.yml", "- apt: [\n");
        let err = union_of(&dir, "- hosts: all\n  roles: [r]\n").unwrap_err();
        assert!(format!("{err:#}").contains("broken.yml"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run that names no Python module builds nothing, so a controller without ansible-core
    /// runs native playbooks exactly as before.
    ///
    /// What would make this red: the helper started for every run, which would refuse every
    /// playbook on a machine that has no ansible-core - including the ones that never needed it.
    #[test]
    fn a_run_with_no_python_module_builds_nothing() {
        let none = union_for(&std::collections::BTreeSet::new(), &[]).expect("nothing to build");
        assert!(none.is_none());
    }

    /// The blob is named by the hash of the zip's own bytes, which is the name the agent
    /// computes for the same blob when it decides whether it already holds it.
    ///
    /// What would make this red: hashing the base64 text instead of what it encodes. Both are
    /// 64 hex characters and both look like a working cache key, and the two sides would then
    /// never agree on what a blob is called: every run would upload a payload the agent already
    /// has, and no `has_blob` would ever answer yes. The vector is the agent's own,
    /// `UEsDBA==` for `PK\x03\x04`.
    #[test]
    fn the_blob_is_named_by_the_hash_of_the_zip_not_of_its_base64() {
        let answer = br#"{"zip_b64": "UEsDBA==", "modules": {"ping": {"module_fqn":
            "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0,
            "extensions": {}}}}"#;
        let union = exchange(Vec::new(), framed(answer), &["ping".to_string()]).unwrap();
        assert_eq!(union.hash, blake3::hash(b"PK\x03\x04").to_hex().to_string());
        assert_ne!(union.hash, blake3::hash(b"UEsDBA==").to_hex().to_string());
        // The shape the agent refuses a name by before it will look for a file under it.
        assert_eq!(union.hash.len(), 64, "{}", union.hash);
        assert!(
            union
                .hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{}",
            union.hash
        );
    }

    /// A blob that is not base64 is refused on the controller, naming where it broke.
    ///
    /// What would make this red: decoding what decodes and dropping the rest, which names the
    /// blob by the hash of bytes that are not the zip the agent will be sent.
    #[test]
    fn a_blob_that_is_not_base64_is_refused() {
        let answer = br#"{"zip_b64": "UEsD!BA==", "modules": {}}"#;
        let err = exchange(Vec::new(), framed(answer), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("base64"), "{err}");
    }

    /// A truncated set of facts is an error rather than a default. What would make this red:
    /// filling `profile`, `rlimit_nofile` or `extensions` with a guess, which would run a module
    /// under a serialisation profile, or without an extension, it was not built for.
    #[test]
    fn missing_facts_are_refused_rather_than_guessed() {
        let answer = br#"{"zip_b64": "UEsD", "modules": {"ping": {"module_fqn":
            "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0}}}"#;
        let err = exchange(Vec::new(), framed(answer), &["ping".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("extensions"), "{err}");
        assert!(err.contains("ping"), "{err}");
        let answer = br#"{"zip_b64": "UEsD", "modules": {"ping": {"profile": "legacy"}}}"#;
        let err = exchange(Vec::new(), framed(answer), &["ping".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("module_fqn"), "{err}");
        assert!(err.contains("ping"), "{err}");
    }

    /// The helper's own checks, run under whatever `python3` the machine has: no ansible-core
    /// is needed for the merge and the wrapper reading, which is where the union blob is made
    /// or silently spoiled.
    ///
    /// What would make this red: a merge that lets the last module written win over a
    /// conflicting `module_utils` entry, or a wrapper whose fields moved without this noticing.
    #[test]
    fn the_helper_refuses_a_byte_conflict_in_the_union() {
        let out = Command::new("python3")
            .args(["-c", HELPER, "--self-check"])
            .output()
            .expect("python3 is on PATH wherever these tests run");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A `print()` anywhere in the helper lands on stderr, and stdout carries the frame alone.
    /// The helper's `union` is swapped for one that prints first, which is what
    /// `init_plugin_loader()` or a collection imported under it would do.
    ///
    /// What would make this red: `sys.stdout` left pointing at the frame pipe, so the controller
    /// reads `nois` as a four-byte length and the rest as a frame that is not one.
    #[test]
    fn a_print_in_the_helper_never_reaches_the_frames() {
        use std::io::Write as _;
        const DRIVER: &str = "import sys\nns = {'__name__': 'helper'}\nexec(sys.argv[1], ns)\nns['union'] = lambda modules: (print('noise from a plugin loader'), {'zip_b64': ''})[1]\nns['main']()\n";
        let mut child = Command::new("python3")
            .args(["-c", DRIVER, HELPER])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("python3 is on PATH wherever these tests run");
        let mut request = Vec::new();
        write_frame(&mut request, br#"{"modules": ["ping"]}"#).unwrap();
        child.stdin.take().unwrap().write_all(&request).unwrap();
        let out = child.wait_with_output().unwrap();
        let mut stdout = Cursor::new(out.stdout);
        let frame = read_frame(&mut stdout)
            .expect("stdout starts with a frame")
            .expect("one frame");
        assert_eq!(frame, br#"{"zip_b64": ""}"#);
        assert_eq!(
            stdout.position() as usize,
            stdout.get_ref().len(),
            "nothing but the frame on stdout"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("noise from a plugin loader"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A request and its answer over the helper's own pipes, which is the only place the two
    /// sides of the frame header meet.
    ///
    /// What would make this red: either end writing the length in the other's byte order, or
    /// the helper printing anything at all on stdout beside its frame - both would be invisible
    /// to the tests that feed a hand-built frame in. The machine deciding which branch runs is
    /// whether its `python3` has ansible-core; measured, the dev host's has not.
    #[test]
    fn the_helper_answers_one_frame_over_its_own_pipes() {
        let mut builder = PythonBuilder::under("python3").expect("python3 is on PATH");
        match builder.union(&["ping".to_string()]) {
            Ok(union) => assert!(!union.zip_b64.is_empty(), "an empty blob was accepted"),
            Err(refused) => assert!(
                refused.to_string().contains("could not build the modules"),
                "{refused}"
            ),
        }
    }

    fn framed(payload: &[u8]) -> Cursor<Vec<u8>> {
        let mut buf = Vec::new();
        write_frame(&mut buf, payload).unwrap();
        Cursor::new(buf)
    }
}
