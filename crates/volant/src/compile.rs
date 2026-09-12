// SPDX-License-Identifier: GPL-3.0-or-later
//! Turns a play into the flat list of steps the executor walks.
//!
//! A play is written as a tree - blocks inside blocks, each passing its keywords down - and run
//! as a sequence. Flattening it once, before the first connection, means every host walks the
//! same numbered list, and the coordinator can talk about "step 7" to all of them at once. What
//! the tree said about grouping survives as a span per block plus the section each step belongs
//! to, and [`after`] reads those to decide where a host goes next.
//!
//! The dangerous half of a flattening is the step nobody runs: an index skipped silently is a
//! run that reports success having done less than the playbook asked for. Two things guard it.
//! Nothing here invents an index - every one returned names a step this compilation laid out -
//! and the driver tells the coordinator about every index it steps over, so a step left out is
//! a step the recap can still account for.

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::bail;
use serde_json::{Map, Value};
use volant_protocol::modules::short_name;

use crate::playbook::{Block, Play, PlayTask, TaskOrBlock};
use crate::roles::{RoleEntry, RoleSearch, RoleVars};
use crate::stats::Refusal;

/// Which tasks `--tags` and `--skip-tags` leave in.
///
/// This is ansible-core's own tag algebra, read off `Taggable.evaluate_tags` on 2.19.12 and
/// measured case by case against it. Three things in it are easy to get wrong and each one was
/// measured: a task carrying `always` survives every `--tags`, and survives `--skip-tags all`
/// unless `always` is itself skipped; a task carrying `never` is left out of `--tags all` and of
/// `--tags tagged`, and comes back only when something names it; and a task carrying no tag at
/// all is not an empty set there but the one-element set `{untagged}`, which is why
/// `--tags untagged` selects it and `--skip-tags untagged` drops it.
///
/// One selection serves the run and the four listings, so `--tags x` cannot show one list and
/// run another.
#[derive(Debug, Clone)]
pub(crate) struct TagSelection {
    run: Vec<String>,
    skip: Vec<String>,
}

/// What the reference calls a task with no tags of its own.
const UNTAGGED: &str = "untagged";

impl TagSelection {
    /// An empty `--tags` is the pseudo-tag `all`, which the reference substitutes before a play
    /// ever sees the list. Doing it in the constructor rather than at each call site is what
    /// keeps `never` out of a run nobody narrowed: with an empty list the first half of
    /// [`TagSelection::selects`] does not run at all, and a `never` task would stay in.
    pub fn new(run: Vec<String>, skip: Vec<String>) -> Self {
        TagSelection {
            run: if run.is_empty() {
                vec!["all".to_string()]
            } else {
                run
            },
            skip,
        }
    }

    pub fn selects(&self, tags: &[String]) -> bool {
        let own: &[String] = tags;
        let untagged = own.is_empty() || (own.len() == 1 && own[0] == UNTAGGED);
        // An empty tag list reads as `{untagged}`, exactly as it does in the reference.
        let carries = |name: &str| {
            if own.is_empty() {
                name == UNTAGGED
            } else {
                own.iter().any(|t| t == name)
            }
        };
        let names = |list: &[String]| list.iter().any(|t| carries(t));
        let listed = |list: &[String], name: &str| list.iter().any(|t| t == name);

        // No emptiness test on the run list: `new` put `all` in an empty one, so the selecting
        // half always applies. Guarding it would be a branch that cannot be taken and would read
        // as if the dangerous value were still reachable.
        let mut run = carries("always")
            || (listed(&self.run, "all") && !carries("never"))
            || names(&self.run)
            || (listed(&self.run, "tagged") && !untagged && !carries("never"));
        if run && !self.skip.is_empty() {
            if listed(&self.skip, "all") {
                // `--skip-tags all` is the one place `always` survives a skip, and it stops
                // surviving the moment `always` is named alongside it.
                run = carries("always") && !listed(&self.skip, "always");
            } else if names(&self.skip) || (listed(&self.skip, "tagged") && !untagged) {
                run = false;
            }
        }
        run
    }
}

/// What one step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepKind {
    /// A module to run on the host.
    Task,
    /// `meta`: something asked of the engine rather than of the host. It shows a banner and
    /// reports nothing, which is why it is the one step kind that prints once per live host.
    Meta,
    /// A flush point: where the notified handlers run. `explicit` is set for the one a playbook
    /// wrote as `meta: flush_handlers`, which shows the same `TASK [meta]` banner every other
    /// `meta` shows; the three the compiler puts in itself show nothing, measured.
    ///
    /// It is the one step whose *successors* are not known when the play is compiled: the
    /// coordinator inserts the handler steps behind it once every host has reported it, so a
    /// driver that has reported it waits before reading the list again.
    Flush { explicit: bool },
    /// One handler of [`Compiled::handlers`], spliced in behind a flush point. It runs for a host
    /// that notified it and reports nothing for a host that did not.
    Handler(usize),
}

/// One handler, with everything a flush needs to decide whether to run it and what to call it.
#[derive(Debug, Clone)]
pub(crate) struct Handler {
    pub task: PlayTask,
    /// The extra names this handler answers to. Measured on ansible-core 2.19.12: `listen` works
    /// for a name no handler carries as its `name`, and two handlers listening to one name both
    /// run, in definition order.
    pub listen: Vec<String>,
    /// The role instance it came from, which is what puts `base : ` in front of its banner and
    /// what decides which variable layers it reads.
    pub role: Option<usize>,
}

/// Which of a block's three lists a step came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Body,
    Rescue,
    Always,
}

#[derive(Debug, Clone)]
pub(crate) struct Step {
    pub kind: StepKind,
    /// The task with every keyword inherited from the blocks above it already merged in, so
    /// nothing downstream has to remember it was ever inside anything.
    pub task: PlayTask,
    /// The innermost block this step belongs to, if any.
    pub block: Option<usize>,
    pub section: Section,
    /// The role instance this step came from, if any: an index into [`Compiled::roles`]. It
    /// decides two things, both measured - the `role : task` prefix the banner carries, and
    /// which variable layers the driver puts under the play's.
    pub role: Option<usize>,
}

/// Where one block's three sections landed in the flat list.
#[derive(Debug, Clone)]
pub(crate) struct BlockSpan {
    pub body: Range<usize>,
    pub rescue: Range<usize>,
    pub always: Range<usize>,
    pub parent: Option<usize>,
    /// Which section of `parent` this block sits in. Kept here rather than worked back out of
    /// the indices: reading it off the step before the one being left is the kind of arithmetic
    /// that quietly points one past a span.
    pub section: Section,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Compiled {
    pub steps: Vec<Step>,
    pub blocks: Vec<BlockSpan>,
    /// One entry per role instance the play runs, in the order they run. A step naming one
    /// reads its variables from here; a step naming none reads [`Compiled::exported`].
    pub roles: Vec<RoleVars>,
    /// What every role in the play lends to everything else in it.
    ///
    /// Measured on ansible-core 2.19.12: a role's `defaults` and `vars` are visible in the
    /// play's `pre_tasks`, which run **before** the role, in its `tasks` and `post_tasks`, and
    /// inside the other roles - a role early in the list sees a variable only a later one sets.
    /// So this is one map per layer for the whole play, not something that accumulates as the
    /// list is walked, and a role's own values are laid over it for that role's own steps.
    pub exported: RoleVars,
    /// Every handler the play can reach, in the order they run: each role's, in the order the
    /// roles run, then the play's own. Measured on ansible-core 2.19.12 - handlers run in
    /// **definition** order and not in notification order, and a role's run before the play's at
    /// the same flush.
    pub handlers: Vec<Handler>,
}

impl Compiled {
    /// Inserts `steps` at `at`, growing every block range that ends at or after `at` and shifting
    /// every range that starts at or after it.
    ///
    /// Called by the coordinator alone, at a flush point, once every host has reported that step
    /// and before any of them reads the list again - which is what keeps the index each driver
    /// holds pointing at the step it was pointing at. A range that *ends* at `at` has to grow:
    /// `at` is one past the flush, so a flush written last in a block's body ends that body
    /// exactly there, and testing `>` instead of `>=` would drop the spliced handlers into that
    /// block's rescue, where a host that failed nothing never goes.
    pub(crate) fn splice(&mut self, at: usize, steps: Vec<Step>) {
        let n = steps.len();
        for span in &mut self.blocks {
            for range in [&mut span.body, &mut span.rescue, &mut span.always] {
                if range.start >= at {
                    range.start += n;
                }
                if range.end >= at {
                    range.end += n;
                }
            }
        }
        self.steps.splice(at..at, steps);
    }
}

/// The handlers one name notifies, in definition order: the **first** one carrying it as its
/// `name`, and **every** one listening for it.
///
/// The asymmetry is measured on ansible-core 2.19.12, both halves on their own: two handlers
/// written with the same name run the first alone, and a handler named `x` beside one listening
/// for `x` both run when `x` is notified. It is what makes a role that runs three times
/// contribute one handler rather than three - the three copies share a name, and only the first
/// of them is ever reached.
pub(crate) fn resolve_notify(c: &Compiled, name: &str) -> Vec<usize> {
    let named = c.handlers.iter().position(|h| h.task.name == name);
    c.handlers
        .iter()
        .enumerate()
        .filter(|(i, h)| named == Some(*i) || h.listen.iter().any(|l| l == name))
        .map(|(i, _)| i)
        .collect()
}

/// The steps a flush point at `at` inserts behind itself: one per handler, in definition order.
///
/// All of them, not only the notified ones, because the coordinator decides this for every host
/// at once and which handlers a host notified is the driver's own business. A step for a handler
/// the host did not notify reports nothing and shows nothing.
///
/// They carry the flush step's own block and section, which is what makes the spliced steps part
/// of the section the flush was written in: measured on ansible-core 2.19.12, a handler that
/// fails at a flush written inside a block is taken by that block's `rescue` (`rescued=1`, exit
/// 0), and a host that never entered a rescue holding the flush steps over the handlers too.
pub(crate) fn handler_steps(c: &Compiled, at: usize) -> Vec<Step> {
    let (block, section) = (c.steps[at].block, c.steps[at].section);
    c.handlers
        .iter()
        .enumerate()
        .map(|(i, h)| Step {
            kind: StepKind::Handler(i),
            task: h.task.clone(),
            block,
            section,
            role: h.role,
        })
        .collect()
}

/// Every `meta` action ansible-core 2.19.12 accepts, and whether this release honours it.
///
/// `noop` does nothing here and nothing there, and `flush_handlers` compiles into a flush point
/// of its own. The rest ask for something this release cannot do and are refused by their own
/// names.
pub(crate) const META_ACTIONS: &[(&str, bool)] = &[
    ("clear_facts", false),
    ("clear_host_errors", false),
    ("end_host", false),
    ("end_play", false),
    ("flush_handlers", true),
    ("noop", true),
    ("refresh_inventory", false),
    ("reset_connection", false),
];

/// The `meta` action that runs the notified handlers where it stands.
pub(crate) const FLUSH_HANDLERS: &str = "flush_handlers";

/// The action a `meta` task asked for.
pub(crate) fn meta_action(task: &PlayTask) -> &str {
    task.args
        .get("_raw_params")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

/// How deep roles and imported files may nest before the run is refused: the same ceiling a
/// cycle through an inventory's `:children` is given, for the same reason - a cycle here reads
/// files and recurses on what it finds until the stack is gone.
///
/// One counter for all three recursions - a `meta/main.yml` dependency, an `import_role` inside
/// a role's tasks, an `import_tasks` inside an imported file - because a cycle can run through
/// any mixture of them, and a counter that only one of them increments bounds nothing.
const DEPTH: usize = 32;

/// What the reference says when two roles import each other, measured on ansible-core 2.19.12,
/// exit 1.
///
/// A file importing itself with `import_tasks` is not caught there at all: the interpreter runs
/// out of stack and `ansible-playbook` exits **250** printing a Python traceback under
/// `Unexpected Exception, this is probably a bug`. This engine refuses that shape too, at exit 1
/// and in its own words - the divergence is deliberate, since 250 is the reference reporting its
/// own crash and there is no traceback here to go with it.
const RECURSION: &str = "A recursion loop was detected with the roles specified. Make sure child roles do not have dependencies on parent roles: maximum recursion depth exceeded";

/// The role instances a play has already run.
///
/// The identity of a role instance is **not** its name: measured on ansible-core 2.19.12,
/// `- base` written twice runs once, while `- base` and `- { role: base, p: v }` run twice, and
/// so do `- base` and `- { role: base, vars: { p: v } }`. So everything the entry says about what
/// the role will do goes into the identity - its parameters, its `vars:`, its `when:` and which
/// files of the role it asked for. Comparing names alone drops the second entry, and a run that
/// skipped a role the playbook asked for reports success having done less than it says.
type Seen = Vec<(String, Value)>;

fn identity(entry: &RoleEntry) -> (String, Value) {
    (
        entry.name.clone(),
        serde_json::json!({
            "params": sorted(&entry.params),
            "vars": sorted(&entry.keywords.vars),
            "when": entry.keywords.when,
            "from": [
                &entry.from.tasks,
                &entry.from.vars,
                &entry.from.defaults,
                &entry.from.handlers,
            ],
        }),
    )
}

/// A mapping as a value that compares by content rather than by the order the YAML happened to
/// list the keys in: two entries writing the same parameters in a different order are the same
/// entry.
fn sorted(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut pairs: Vec<(&String, &Value)> = map.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
}

struct Builder<'a> {
    steps: Vec<Step>,
    blocks: Vec<BlockSpan>,
    roles: Vec<RoleVars>,
    /// Every role's handlers, in the order the roles run. The play's own are appended behind
    /// them, once, when the walk is over.
    handlers: Vec<Handler>,
    search: &'a RoleSearch,
    /// The directory relative paths are read against: the playbook's, then a role's `tasks/`
    /// while its tasks are being compiled, then the directory of an imported file while its own
    /// imports are being read. Measured: `import_tasks: sub/extra.yml` inside a role's
    /// `tasks/main.yml` reads `<role>/tasks/sub/extra.yml`, and an `import_tasks` inside **that**
    /// file reads it beside itself.
    file_dir: PathBuf,
    /// The `tasks/` directory of the role being compiled, if any: the first place an
    /// `import_tasks` written inside a role looks.
    role_tasks: Option<PathBuf>,
    /// How many roles and imported files are open above whatever is being compiled now.
    depth: usize,
    /// Which tasks `--tags` and `--skip-tags` leave in. A task the selection drops is never
    /// pushed, rather than pushed and filtered out afterwards: a `retain` over the finished
    /// list would leave every `BlockSpan` pointing at the indices the list used to have, and a
    /// span one step wide of the wrong step is how a host walks into a section it never entered.
    selection: &'a TagSelection,
}

impl Builder<'_> {
    /// The one place a step is added, so the tag selection cannot be honoured on one path and
    /// forgotten on another.
    fn push(&mut self, step: Step) {
        if self.selection.selects(&step.task.tags) {
            self.steps.push(step);
        }
    }

    /// One of the three flush points the compiler puts in itself, at the end of a section that
    /// has something in it: `mark` is where the section started, so a section that laid out no
    /// step gets no flush point. Nothing there could have notified anything, and a step every
    /// host has to report through is a barrier.
    ///
    /// Pushed past [`Builder::push`] on purpose: it is the engine's own structure and not
    /// something the playbook wrote, so no `--tags` may drop it. A flush point the selection
    /// removed would leave the handlers a selected task notified with nowhere to run.
    fn flush_point(&mut self, mark: usize) {
        if self.steps.len() == mark {
            return;
        }
        self.steps.push(Step {
            kind: StepKind::Flush { explicit: false },
            task: PlayTask::empty(),
            block: None,
            section: Section::Body,
            role: None,
        });
    }

    fn items(
        &mut self,
        items: &[TaskOrBlock],
        inherited: &PlayTask,
        block: Option<usize>,
        section: Section,
        role: Option<usize>,
    ) -> anyhow::Result<()> {
        for item in items {
            match item {
                TaskOrBlock::Task(task) => self.task(task, inherited, block, section, role)?,
                TaskOrBlock::Block(inner) => {
                    self.block(inner, inherited, block, section, role)?;
                }
            }
        }
        Ok(())
    }

    /// One task, unless it is one of the three import statements - those name work rather than
    /// being it, and what they name is spliced here, where the task would have gone.
    fn task(
        &mut self,
        task: &PlayTask,
        inherited: &PlayTask,
        block: Option<usize>,
        section: Section,
        role: Option<usize>,
    ) -> anyhow::Result<()> {
        match short_name(&task.module) {
            "import_tasks" => return self.import_tasks(task, inherited, block, section, role),
            "import_role" => {
                let entry = import_role_entry(task)?;
                // A fresh `seen` list: measured, `import_role` runs its role even when the
                // play's `roles:` list has already run it with the same parameters. Its
                // dependencies still deduplicate against each other.
                return self.role(
                    &entry,
                    &merge(inherited, task),
                    &mut Vec::new(),
                    block,
                    section,
                );
            }
            // Measured: written as a task rather than in the list of plays, the reference fails
            // the task with `Action 'ansible.builtin.import_playbook' does not support raw
            // params.`, exit 2. This refuses before the play instead of during it, so nothing
            // has run when the operator reads it, but the code is the one their scripts read.
            "import_playbook" => {
                return Err(Refusal::at(
                    2,
                    "Task failed: Action 'ansible.builtin.import_playbook' does not support raw params.",
                ));
            }
            _ => {}
        }
        let task = merge(inherited, task);
        let kind = match crate::playbook::is_meta(&task) {
            true if meta_action(&task) == FLUSH_HANDLERS => StepKind::Flush { explicit: true },
            true => StepKind::Meta,
            false => StepKind::Task,
        };
        self.push(Step {
            kind,
            task,
            block,
            section,
            role,
        });
        Ok(())
    }

    /// `import_tasks`: the file's own task list, compiled where the statement stands, with the
    /// statement's keywords folded into every task of it.
    fn import_tasks(
        &mut self,
        task: &PlayTask,
        inherited: &PlayTask,
        block: Option<usize>,
        section: Section,
        role: Option<usize>,
    ) -> anyhow::Result<()> {
        // Measured, exit 4: the reference refuses this before anything runs rather than looping
        // over a statement that was resolved once at compile time.
        if task.loop_items.is_some() {
            bail!(
                "You cannot use loops on 'import_tasks' statements. You should use 'include_tasks' instead."
            );
        }
        unknown_options("import_tasks", &task.args, &["file", "_raw_params"], &[])?;
        let name = task
            .args
            .get("file")
            .or_else(|| task.args.get("_raw_params"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("'import_tasks' takes a file name"))?;
        // Inside a role a bare name has two homes and the reference takes the first that exists:
        // the directory of the file that wrote the statement, then the role's own `tasks/`.
        // Measured on ansible-core 2.19.12 with a role whose `tasks/sub/deep.yml` imports
        // `shared.yml`, in all three states - only `tasks/shared.yml`, only `tasks/sub/
        // shared.yml`, both - and the one beside the importer wins whenever it is there. The
        // fallback is what a role like UBUNTU22-CIS needs: `tasks/section_1/cis_1.1.1.x.yml`
        // writes `import_tasks: file: warning_facts.yml` and means `tasks/warning_facts.yml`,
        // so trying the importer's directory alone refuses a role the reference compiles.
        let beside = self.file_dir.join(name);
        let path = match &self.role_tasks {
            Some(tasks) if !beside.is_file() && tasks.join(name).is_file() => tasks.join(name),
            _ => beside,
        };
        if !path.is_file() {
            // Exit 1, measured, with the reference's own two sentences. The reference adds a
            // third naming Python's own errno, which this engine has nothing to say about.
            return Err(Refusal::at(
                1,
                format!(
                    "Unable to retrieve file contents.\nCould not find or access '{}' on the Ansible Controller.",
                    path.display()
                ),
            ));
        }
        // A file that imports itself, directly or around a ring of files, otherwise reads and
        // recurses until the stack is gone: no recap, no exit code, nothing to read. Refused
        // here instead, at the depth every other recursion in this compilation shares.
        self.depth += 1;
        if self.depth > DEPTH {
            return Err(Refusal::at(
                1,
                format!(
                    "imports nest deeper than {DEPTH} levels at '{}': a file that imports itself",
                    path.display()
                ),
            ));
        }
        let imported = crate::playbook::parse_tasks_file(&path)?;
        let merged = merge(inherited, task);
        // The imported file's own directory, for the imports it carries itself, put back
        // afterwards so the rest of the importing file still reads against its own.
        let previous = std::mem::replace(
            &mut self.file_dir,
            path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        );
        let result = self.items(&imported, &merged, block, section, role);
        self.file_dir = previous;
        self.depth -= 1;
        result
    }

    /// One role entry: its dependencies first, then its argument-spec check, then its tasks.
    ///
    /// Measured on ansible-core 2.19.12: a `meta/main.yml` dependency runs before the role that
    /// depends on it, with its own parameters, and a dependency two roles share runs once.
    fn role(
        &mut self,
        entry: &RoleEntry,
        inherited: &PlayTask,
        seen: &mut Seen,
        block: Option<usize>,
        section: Section,
    ) -> anyhow::Result<()> {
        let path = self.search.locate(&entry.name)?;
        let role = crate::roles::load(&path, &entry.from)?;
        let key = identity(entry);
        if !role.allow_duplicates && seen.contains(&key) {
            return Ok(());
        }
        seen.push(key);
        // Two roles that import each other never repeat an identity - `import_role` starts a
        // `seen` list of its own, as measured - so the dedup above stops nothing and only the
        // depth does. The reference's own sentence, at the exit 1 measured with it.
        self.depth += 1;
        if self.depth > DEPTH {
            return Err(Refusal::at(1, RECURSION));
        }
        let kw = merge(inherited, &entry.keywords);
        for dependency in &role.dependencies {
            self.role(dependency, &kw, seen, block, section)?;
        }
        let index = self.roles.len();
        self.roles.push(RoleVars {
            name: entry.name.clone(),
            defaults: role.defaults,
            vars: role.vars,
            params: entry.params.clone(),
        });
        // The role's handlers, before its tasks are walked, so a role imported by another one
        // contributes its handlers in front of the importer's. Their keywords are merged the way
        // a task's are: a `when` or a `become` written on the role entry reaches them too.
        for handler in role.handlers {
            self.handlers.push(Handler {
                task: merge(&kw, &handler.task),
                listen: handler.listen,
                role: Some(index),
            });
        }
        if let Some(spec) = role.argument_spec {
            let task = merge(
                &kw,
                &validation(entry, &path, spec, role.argument_spec_description),
            );
            self.push(Step {
                kind: StepKind::Task,
                task,
                block,
                section,
                role: Some(index),
            });
        }
        let previous = std::mem::replace(&mut self.file_dir, path.join("tasks"));
        let outer_role = self.role_tasks.replace(path.join("tasks"));
        let result = self.items(&role.tasks, &kw, block, section, Some(index));
        self.file_dir = previous;
        self.role_tasks = outer_role;
        self.depth -= 1;
        result
    }

    fn block(
        &mut self,
        b: &Block,
        inherited: &PlayTask,
        parent: Option<usize>,
        section: Section,
        role: Option<usize>,
    ) -> anyhow::Result<()> {
        let merged = merge(inherited, &b.keywords);
        // Reserved before the sections are walked, so a block nested inside this one gets an
        // identifier of its own and can name this one as its parent.
        let id = self.blocks.len();
        self.blocks.push(BlockSpan {
            body: 0..0,
            rescue: 0..0,
            always: 0..0,
            parent,
            section,
        });
        let start = self.steps.len();
        self.items(&b.body, &merged, Some(id), Section::Body, role)?;
        let rescue = self.steps.len();
        self.items(&b.rescue, &merged, Some(id), Section::Rescue, role)?;
        let always = self.steps.len();
        self.items(&b.always, &merged, Some(id), Section::Always, role)?;
        let end = self.steps.len();
        self.blocks[id].body = start..rescue;
        self.blocks[id].rescue = rescue..always;
        self.blocks[id].always = always..end;
        Ok(())
    }
}

/// The check the reference inserts in front of a role that ships `meta/argument_specs.yml`.
///
/// Measured: the banner reads `TASK [<role> : Validating arguments against arg spec '<entry>']`,
/// the task runs on the controller, and a failure carries `argument_errors`,
/// `argument_spec_data` and a `validate_args_context` naming the role and its absolute path.
/// A spec carrying a `short_description` puts it after the entry name behind a dash, measured
/// through `--list-tasks`: `Validating arguments against arg spec 'main' - The spec role`.
fn validation(
    entry: &RoleEntry,
    path: &Path,
    spec: Map<String, Value>,
    description: Option<String>,
) -> PlayTask {
    let name = match description {
        Some(text) => format!(
            "Validating arguments against arg spec '{}' - {text}",
            entry.from.tasks
        ),
        None => format!(
            "Validating arguments against arg spec '{}'",
            entry.from.tasks
        ),
    };
    let mut args = Map::new();
    args.insert("argument_spec".to_string(), Value::Object(spec));
    args.insert(
        "provided_arguments".to_string(),
        Value::Object(entry.params.clone()),
    );
    args.insert(
        "validate_args_context".to_string(),
        serde_json::json!({
            "argument_spec_name": entry.from.tasks,
            "name": entry.name,
            "path": path.display().to_string(),
            "type": "role",
        }),
    );
    PlayTask {
        name,
        named: true,
        module: "validate_argument_spec".to_string(),
        args,
        // Measured on ansible-core 2.19.12: the check lists as
        // `TAGS: [always, <the role entry's tags>]`, so it carries `always` and runs whatever
        // `--tags` asks for - a role's arguments are validated before its tasks or not at all.
        tags: vec!["always".to_string()],
        ..PlayTask::empty()
    }
}

/// Refuses an argument the statement does not take, and one it takes but this release cannot
/// honour.
///
/// Measured on ansible-core 2.19.12: `import_role: { name: x, typo: v }` is refused with
/// `Invalid options for import_role: typo`, exit 4, and `import_tasks` with `apply:` the same
/// way. An argument read and dropped is the failure family this engine is written against - the
/// run would report success having done something other than what was asked - so the ones the
/// reference accepts and this release has nothing to do with are refused by their own names
/// rather than ignored.
fn unknown_options(
    statement: &str,
    args: &Map<String, Value>,
    known: &[&str],
    unsupported: &[&str],
) -> anyhow::Result<()> {
    let mut invalid: Vec<&str> = args
        .keys()
        .map(String::as_str)
        .filter(|key| !known.contains(key))
        .collect();
    invalid.sort_unstable();
    if !invalid.is_empty() {
        bail!("Invalid options for {statement}: {}", invalid.join(", "));
    }
    if let Some(key) = unsupported.iter().find(|key| args.contains_key(**key)) {
        bail!("'{statement}' option '{key}' is not supported yet");
    }
    Ok(())
}

/// The role one `import_role` task names, read out of its arguments.
fn import_role_entry(task: &PlayTask) -> anyhow::Result<RoleEntry> {
    // Measured: the reference takes these nine and refuses anything else. The last three name
    // work this release does not do - `public` exports nothing of its own, nothing dedups an
    // `import_role`, and the argument-spec check has no off switch - so they are refused rather
    // than accepted and forgotten.
    unknown_options(
        "import_role",
        &task.args,
        &[
            "name",
            "role",
            "tasks_from",
            "vars_from",
            "defaults_from",
            "handlers_from",
            "public",
            "allow_duplicates",
            "rolespec_validate",
        ],
        &["public", "allow_duplicates", "rolespec_validate"],
    )?;
    let text = |key: &str| task.args.get(key).and_then(Value::as_str);
    let name = text("name")
        .or_else(|| text("role"))
        .ok_or_else(|| anyhow::anyhow!("'import_role' takes a role name"))?;
    let from = |key: &str, fallback: &str| text(key).unwrap_or(fallback).to_string();
    Ok(RoleEntry {
        name: name.to_string(),
        from: crate::roles::RoleFrom {
            tasks: from("tasks_from", "main"),
            vars: from("vars_from", "main"),
            defaults: from("defaults_from", "main"),
            handlers: from("handlers_from", "main"),
        },
        // An `import_role` takes no free keys: everything it says is an argument of the
        // statement, and what it hands the role travels in the task's own `vars:`, which the
        // merge already carries down to every task of the role.
        params: Map::new(),
        keywords: PlayTask::empty(),
    })
}

/// A play's sections, flattened into one numbered list.
///
/// The order is the reference's own, measured: `pre_tasks`, then the roles with each one's
/// dependencies in front of it, then `tasks`, then `post_tasks`.
///
/// The play's own `become` and `become_user` are deliberately left out of the merge: their
/// precedence against a host variable was measured in an earlier release and lives in the
/// executor, which reads them from the play. A block is a task keyword, so its `become` does
/// come down here.
pub(crate) fn compile(
    play: &Play,
    search: &RoleSearch,
    selection: &TagSelection,
) -> anyhow::Result<Compiled> {
    let mut builder = Builder {
        steps: Vec::new(),
        blocks: Vec::new(),
        roles: Vec::new(),
        handlers: Vec::new(),
        search,
        file_dir: play.dir.clone(),
        role_tasks: None,
        depth: 0,
        selection,
    };
    // The play's own tags are the outermost layer of the merge, so every task under it - in a
    // role, in an imported file, at any block depth - carries them for the selection and for
    // the listing. Measured: `--skip-tags <play tag>` leaves a play with nothing to do.
    let empty = PlayTask {
        tags: play.tags.clone(),
        ..PlayTask::empty()
    };
    // A flush point closes each of the three sections. Measured on ansible-core 2.19.12: the
    // implicit ones are after `pre_tasks`, after the roles and `tasks` **together**, and after
    // `post_tasks`, and what they run counts `ok`/`changed` in the recap like any other task.
    //
    // Laid out here, as the walk reaches each boundary, rather than inserted afterwards: a step
    // put into the finished list would grow the span of a block that happens to end exactly
    // there, and a host draining that block's `always` after a failure would then walk into a
    // flush it has no business in. Whether the play has a handler at all is not known yet - a
    // role read below contributes its own - so all three are always there. A play with nothing
    // to flush pays three steps that report and show nothing.
    let mut mark = builder.steps.len();
    builder.items(&play.pre_tasks, &empty, None, Section::Body, None)?;
    builder.flush_point(mark);
    mark = builder.steps.len();
    let mut seen: Seen = Vec::new();
    for entry in &play.roles {
        builder.role(entry, &empty, &mut seen, None, Section::Body)?;
    }
    builder.items(&play.tasks, &empty, None, Section::Body, None)?;
    builder.flush_point(mark);
    mark = builder.steps.len();
    builder.items(&play.post_tasks, &empty, None, Section::Body, None)?;
    builder.flush_point(mark);
    // The play's own handlers run behind every role's, measured.
    for handler in &play.handlers {
        builder.handlers.push(Handler {
            task: handler.task.clone(),
            listen: handler.listen.clone(),
            role: None,
        });
    }

    // Every role lends its `defaults` and its `vars` to the whole play, so the two layers are
    // folded once here rather than accumulated as the list is walked, and each role's own values
    // are laid back over them for its own steps. Measured both ways: a role sees a variable only
    // a later role sets, and a role whose own `vars` name it keeps its own value.
    let mut exported = RoleVars::default();
    for role in &builder.roles {
        extend(&mut exported.defaults, &role.defaults);
    }
    for role in &builder.roles {
        extend(&mut exported.vars, &role.vars);
    }
    let mut roles = builder.roles;
    for role in &mut roles {
        role.defaults = layered(&exported.defaults, &role.defaults);
        role.vars = layered(&exported.vars, &role.vars);
    }
    Ok(Compiled {
        steps: builder.steps,
        blocks: builder.blocks,
        roles,
        exported,
        handlers: builder.handlers,
    })
}

fn extend(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
}

fn layered(under: &Map<String, Value>, over: &Map<String, Value>) -> Map<String, Value> {
    let mut out = under.clone();
    extend(&mut out, over);
    out
}

/// One task with the keywords of everything above it folded in. The inner value wins wherever it
/// says anything, and `when` concatenates outer first: measured on ansible-core 2.19.12, a block
/// with a false `when` and a task with a true one skips reporting the **block's** condition as
/// the `false_condition`, so the outer condition has to be evaluated first.
fn merge(outer: &PlayTask, inner: &PlayTask) -> PlayTask {
    let mut task = inner.clone();
    task.when = outer
        .when
        .iter()
        .chain(&inner.when)
        .cloned()
        .collect::<Vec<_>>();
    // Measured: a block saying `ignore_errors: true` loses to a task saying `ignore_errors:
    // false` inside it, which is why the keyword is three-state all the way down here.
    task.ignore_errors = inner.ignore_errors.or(outer.ignore_errors);
    task.timeout = inner.timeout.or(outer.timeout);
    task.r#become = inner.r#become.or(outer.r#become);
    task.become_user = inner
        .become_user
        .clone()
        .or_else(|| outer.become_user.clone());
    // Tags are a set, and they only ever grow downwards: measured on ansible-core 2.19.12, a
    // task inside a block tagged `outer` is selected by `--skip-tags outer` as surely as the
    // block is, and it lists as `TAGS: [inner, outer]` with its own. Sorted and deduplicated
    // here because every reader of the list treats it as a set - the listings sort what they
    // print, and the selection asks whether a name is in it.
    task.tags = inner
        .tags
        .iter()
        .chain(&outer.tags)
        .cloned()
        .collect::<Vec<_>>();
    task.tags.sort_unstable();
    task.tags.dedup();
    // A `notify` written on a block reaches the tasks under it - measured on ansible-core
    // 2.19.12, a block carrying `notify: h` runs `h` when a task inside it changes something -
    // and a task that notifies something of its own keeps both. Left in the order they were
    // written, with the task's own first: nothing depends on it, since handlers run in
    // definition order, but a refusal names the first name the playbook wrote.
    for name in &outer.notify {
        if !task.notify.contains(name) {
            task.notify.push(name.clone());
        }
    }
    // The outer map first, so a name the task sets itself keeps the task's value.
    let mut vars = outer.vars.clone();
    vars.extend(inner.vars.clone());
    task.vars = vars;
    // A keyword this release cannot execute has to be refused wherever it was written, and once
    // a role's tasks come from another file the only place the pre-flight still sees them is
    // here. Carrying the outer keyword down means a `tags:` on a role entry, or a `notify:` on a
    // block inside a role, is refused by its own name rather than inherited and ignored.
    for kw in &outer.unsupported {
        if !task.unsupported.contains(kw) {
            task.unsupported.push(kw);
        }
    }
    task.unsupported.sort_unstable();
    task
}

/// The blocks around a step, innermost first, each with the section of that block the step sits
/// in. One walk, so the three functions below decide different things about the same tree
/// rather than each re-deriving it from the indices.
fn ancestors(c: &Compiled, index: usize) -> impl Iterator<Item = (usize, Section)> + '_ {
    let step = &c.steps[index];
    let mut next = step.block.map(|id| (id, step.section));
    std::iter::from_fn(move || {
        let (id, section) = next?;
        let span = &c.blocks[id];
        next = span.parent.map(|parent| (parent, span.section));
        Some((id, section))
    })
}

/// The rescues a host standing on `index` is already inside, innermost first: the sections it
/// may walk through that a host on the happy path steps over.
///
/// Read off the step rather than carried by the host because a step names every block around it
/// and the section it sits in each of them, so the answer is the same whichever way the host
/// arrived. It is the set every seek from that step needs: a cleanup or a recovery written
/// inside a rescue is only reachable for a host that entered that rescue.
fn entered(c: &Compiled, index: usize) -> Vec<usize> {
    ancestors(c, index)
        .filter(|&(_, section)| section == Section::Rescue)
        .map(|(id, _)| id)
        .collect()
}

/// The first index at or after `from` that a host has a reason to run, given the rescues it is
/// already inside, or the length of the list when there is none.
///
/// A step is stepped over exactly when it sits in a `rescue` the host has not entered: rescue is
/// the one section reached only by failing, and a host already working through one carries on
/// to its end. Everything else - a body, an `always`, a block written inside either - is on the
/// way. Nothing here can name a step that does not exist.
fn seek(c: &Compiled, from: usize, entered: &[usize]) -> usize {
    (from..c.steps.len())
        .find(|&i| {
            !ancestors(c, i)
                .any(|(id, section)| section == Section::Rescue && !entered.contains(&id))
        })
        .unwrap_or(c.steps.len())
}

/// Whether a step sits in the `block:` list of every block around it.
///
/// Measured on ansible-core 2.19.12: `--list-tasks` and `--list-tags` walk a block's body and
/// nothing else, so a task written in a `rescue:` or an `always:` is listed nowhere and its tags
/// do not reach `TASK TAGS`. That is the reference's own behaviour, not an omission of ours -
/// which is why the listing asks this rather than filtering by hand.
///
/// A flush point the compiler put in itself is listed nowhere either, and neither is a handler:
/// measured, `--list-tasks` on a playbook full of handlers shows the `meta` a playbook wrote and
/// not one handler. The step is engine structure rather than something the playbook says.
pub(crate) fn listed(c: &Compiled, index: usize) -> bool {
    match c.steps[index].kind {
        StepKind::Flush { explicit: false } | StepKind::Handler(_) => return false,
        _ => {}
    }
    ancestors(c, index).all(|(_, section)| section == Section::Body)
}

/// The step a host starts the play on. Not always the first one: `block: []` with a `rescue`
/// lays that rescue out at index 0, and nothing has failed yet.
pub(crate) fn first(c: &Compiled) -> usize {
    seek(c, 0, &[])
}

/// The index a host moves to after finishing `pos` without failing: the next step it has a
/// reason to run, past the rescue of every block it leaves and of every block it enters. A
/// block whose `block:` list is empty is entered *on* its rescue, so entry needs the same care
/// as exit.
pub(crate) fn after(c: &Compiled, pos: usize) -> usize {
    seek(c, pos + 1, &entered(c, pos))
}

/// Where a host goes when the task at `pos` has just failed and a `rescue` takes that failure:
/// the first step of that rescue, or `None` when nothing rescues it and the host is on its way
/// out of the play.
///
/// The rescue that takes a failure belongs to the innermost enclosing block whose **body** holds
/// the step and whose rescue section has steps in it. A failure raised inside a rescue, or
/// inside an `always`, belongs to whatever encloses that block and never to the block itself -
/// a rescue does not rescue itself, which would be a loop. Measured on ansible-core 2.19.12:
/// a rescue that fails leaves the play through the `always` sections around it (`failed=1
/// rescued=1`), the same failure is taken by a **grandparent's** rescue when there is one
/// (`rescued=2`, exit 0), and a failure raised in a child block's `always` is taken by the
/// parent's rescue with the rest of that `always` left unrun (`rescued=1`, exit 0).
pub(crate) fn rescue_target(c: &Compiled, pos: usize) -> Option<usize> {
    let (id, _) = ancestors(c, pos)
        .find(|&(id, section)| section == Section::Body && !c.blocks[id].rescue.is_empty())?;
    // Entered like any other step: a rescue that opens on a block with an empty `block:` list
    // opens on that block's own rescue, which this failure has not entered. The rescues the
    // host was already inside travel with it, or the recovery of a block written inside one of
    // them would be stepped over on the way in.
    let mut inside = entered(c, pos);
    inside.push(id);
    // No `start < end` guard here, unlike `after_failure` below: measured and deliberate. A
    // rescue whose only step sits inside a nested block's own un-entered rescue - `block: []`
    // there, with a `rescue:` of its own - has nothing to run, and the reference still counts
    // `rescued=1` and exits 0, walking straight into whatever comes after. Guarding this the
    // way `after_failure` does would turn that into an unrescued failure instead.
    Some(seek(c, c.blocks[id].rescue.start, &inside))
}

/// Where a host goes when the task at `pos` has just failed: the `always` section of the
/// innermost block that still has one to run, as the pair (its first step, the index it ends
/// at), or `None` when the play is over for this host.
///
/// Measured on ansible-core 2.19.12: a task failing inside a nested block runs the inner
/// `always`, then the outer `always`, then leaves the play - the step after the outer block
/// never runs and the recap counts the two `always` tasks as `ok`. A task failing **inside** an
/// `always` does not finish that section: the rest of it is skipped and the host leaves, and
/// that holds for a task inside a block written inside the `always` too - measured, a cleanup
/// of two tasks in a nested block whose first task fails runs neither the second nor the step
/// behind the nested block, and the recap reads `failed=2`.
pub(crate) fn after_failure(c: &Compiled, pos: usize) -> Option<(usize, usize)> {
    let inside = entered(c, pos);
    ancestors(c, pos)
        .filter(|&(_, section)| section != Section::Always)
        .map(|(id, _)| {
            let always = &c.blocks[id].always;
            // Entered like any other step, so a cleanup that opens on a block with an empty
            // `block:` list opens on that block's rescue and is not run there either. A
            // section with nothing left to run is no section at all, and the walk goes on.
            //
            // The rescues the host is inside travel with it: a block written in a rescue has
            // its own `always`, and measured on ansible-core 2.19.12 that cleanup runs when
            // the block fails. Seeking with an empty set would step over every one of its
            // steps - the whole cleanup skipped, and nothing said about it.
            (seek(c, always.start, &inside), always.end)
        })
        .find(|&(start, end)| start < end)
}

/// The same, for a host already draining the `always` section that ends at `cleanup`: on
/// through that section, or out into the next one.
///
/// The whole section runs, blocks written inside it included. Which section that is cannot be
/// read off the step: a cleanup task inside a nested block carries its own block's `Body`, and
/// a cleanup that ends where a nested `always` ends is not the end of the section holding it.
/// The host knows which section it entered, so it carries the end of it and this compares
/// against that. Measured on ansible-core 2.19.12: a cleanup made of a nested block of two
/// tasks runs both, and a nested block with an `always` of its own runs that too and then the
/// step behind it, `ok=3 failed=1`.
pub(crate) fn after_pending(c: &Compiled, pos: usize, cleanup: usize) -> Option<(usize, usize)> {
    let next = after(c, pos);
    if next < cleanup {
        return Some((next, cleanup));
    }
    after_failure(c, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playbook::parse;

    fn compiled(text: &str) -> Compiled {
        selected(text, &selection(&[], &[]))
    }

    fn selected(text: &str, selection: &TagSelection) -> Compiled {
        let pb = parse(text, "x.yml").unwrap_or_else(|e| panic!("{e:#}"));
        compile(&pb.plays[0], &RoleSearch::default(), selection).unwrap_or_else(|e| panic!("{e:#}"))
    }

    fn selection(run: &[&str], skip: &[&str]) -> TagSelection {
        TagSelection::new(
            run.iter().map(|s| s.to_string()).collect(),
            skip.iter().map(|s| s.to_string()).collect(),
        )
    }

    /// Whether a step is one of the flush points the compiler puts in itself. They carry no task
    /// and stand at the end of a section, so the helpers below leave them out: what these tests
    /// are about is where the playbook's own steps landed. The flush points have a test of their
    /// own, `each_section_ends_with_a_flush_point_that_nothing_lists`.
    fn structural(step: &Step) -> bool {
        matches!(step.kind, StepKind::Flush { explicit: false })
    }

    fn names(c: &Compiled) -> Vec<&str> {
        c.steps
            .iter()
            .filter(|s| !structural(s))
            .map(|s| s.task.name.as_str())
            .collect()
    }

    /// The index of the step with this name, which is not its position in [`names`] once a play
    /// has a flush point in the middle of it.
    fn index_of(c: &Compiled, name: &str) -> usize {
        c.steps
            .iter()
            .position(|s| s.task.name == name)
            .unwrap_or_else(|| panic!("no step named {name}"))
    }

    /// The order a host walks a compiled play from a given step, on the happy path.
    fn walk(c: &Compiled, from: usize) -> Vec<&str> {
        let mut pos = from;
        let mut seen = Vec::new();
        while pos < c.steps.len() {
            if !structural(&c.steps[pos]) {
                seen.push(c.steps[pos].task.name.as_str());
            }
            pos = after(c, pos);
        }
        seen
    }

    /// The order a host walks the cleanup after the step at `failed` has failed.
    fn walk_failure(c: &Compiled, failed: usize) -> Vec<&str> {
        let mut seen = Vec::new();
        let mut at = after_failure(c, failed);
        while let Some((pos, cleanup)) = at {
            if !structural(&c.steps[pos]) {
                seen.push(c.steps[pos].task.name.as_str());
            }
            at = after_pending(c, pos, cleanup);
        }
        seen
    }

    const NESTED: &str = r#"
- hosts: all
  tasks:
    - name: before
      command: "true"
    - block:
        - name: body one
          command: "true"
        - block:
            - name: nested body
              command: "true"
          rescue:
            - name: nested rescue
              command: "true"
          always:
            - name: nested always
              command: "true"
      rescue:
        - name: outer rescue
          command: "true"
      always:
        - name: outer always
          command: "true"
    - name: after
      command: "true"
"#;

    /// The tree flattens in playbook order, section by section, and every block's span covers
    /// exactly the steps it holds.
    ///
    /// What would make this red: a section laid out in the wrong order, or a span whose end
    /// falls short of the steps it holds - both of which leave a step the driver walks past.
    #[test]
    fn a_nested_play_flattens_in_playbook_order() {
        let c = compiled(NESTED);
        assert_eq!(
            names(&c),
            [
                "before",
                "body one",
                "nested body",
                "nested rescue",
                "nested always",
                "outer rescue",
                "outer always",
                "after",
            ]
        );
        // The outer block is reserved first, so it is block 0 and the nested one is block 1.
        let outer = &c.blocks[0];
        let nested = &c.blocks[1];
        assert_eq!((outer.body.start, outer.body.end), (1, 5));
        assert_eq!((outer.rescue.start, outer.rescue.end), (5, 6));
        assert_eq!((outer.always.start, outer.always.end), (6, 7));
        assert_eq!(outer.parent, None);
        assert_eq!((nested.body.start, nested.body.end), (2, 3));
        assert_eq!((nested.rescue.start, nested.rescue.end), (3, 4));
        assert_eq!((nested.always.start, nested.always.end), (4, 5));
        assert_eq!(nested.parent, Some(0));
        assert_eq!(nested.section, Section::Body);
        // Every span ends inside the list it describes, which is what keeps `after` from
        // naming a step that does not exist.
        for span in &c.blocks {
            assert!(span.always.end <= c.steps.len());
            assert_eq!(span.body.end, span.rescue.start);
            assert_eq!(span.rescue.end, span.always.start);
        }
    }

    /// A host that fails nothing never enters a rescue, at any depth, and leaves each block
    /// through its `always`.
    ///
    /// What would make this red: `after` returning `pos + 1` everywhere, which walks both
    /// rescue sections on the happy path - the failure handling of a playbook running as if it
    /// were ordinary work.
    #[test]
    fn the_happy_path_steps_over_every_rescue() {
        let c = compiled(NESTED);
        assert_eq!(
            walk(&c, 0),
            [
                "before",
                "body one",
                "nested body",
                "nested always",
                "outer always",
                "after",
            ]
        );
    }

    /// A block that ends where its parent's body ends leaves through the parent's `always`, not
    /// through the parent's `rescue`.
    ///
    /// What would make this red: `after` returning the inner block's `always.start` and stopping
    /// there. That index is the parent's rescue when the inner block has no `always` of its own,
    /// so the happy path would run the outer recovery with nothing to recover from.
    #[test]
    fn leaving_a_block_at_the_end_of_a_parent_body_steps_over_the_parent_rescue() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - block:
            - name: inner only
              command: "true"
      rescue:
        - name: outer rescue
          command: "true"
      always:
        - name: outer always
          command: "true"
    - name: after
      command: "true"
"#,
        );
        assert_eq!(
            names(&c),
            ["inner only", "outer rescue", "outer always", "after"]
        );
        assert_eq!(walk(&c, 0), ["inner only", "outer always", "after"]);
    }

    /// A block with neither section behaves like the tasks written out in its place, and a task
    /// outside every block just moves on.
    #[test]
    fn a_bare_block_and_a_bare_task_move_to_the_next_step() {
        let c = compiled(
            "- hosts: all\n  tasks:\n    - block:\n        - name: one\n          command: \"true\"\n    - name: two\n      command: \"true\"\n",
        );
        assert_eq!(walk(&c, 0), ["one", "two"]);
    }

    /// A rescue that is empty leaves `always` starting where the body ended, so the happy path
    /// walks straight into it.
    #[test]
    fn an_empty_rescue_leads_straight_into_always() {
        let c = compiled(
            "- hosts: all\n  tasks:\n    - block:\n        - name: one\n          command: \"true\"\n      always:\n        - name: cleanup\n          command: \"true\"\n",
        );
        assert_eq!(c.blocks[0].rescue, 1..1);
        assert_eq!(walk(&c, 0), ["one", "cleanup"]);
    }

    /// A failure runs the `always` of every block around it, innermost first, and then the play
    /// is over for that host.
    ///
    /// What would make this red: a failure that leaves the play straight away, which skips the
    /// cleanup the playbook wrote; or one that walks into the outer block's body again.
    #[test]
    fn a_failure_runs_every_enclosing_always_and_then_stops() {
        let c = compiled(NESTED);
        // "nested body" fails.
        assert_eq!(walk_failure(&c, 2), ["nested always", "outer always"]);
    }

    /// A failure inside an `always` section does not finish it: measured, the rest of the
    /// section is skipped and the host leaves through whatever is outside.
    #[test]
    fn a_failure_inside_an_always_leaves_the_rest_of_it() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: body
          command: "true"
      always:
        - name: first cleanup
          command: "true"
        - name: second cleanup
          command: "true"
"#,
        );
        let cleanup = c.blocks[0].always.clone();
        assert_eq!(c.steps[cleanup.start].task.name, "first cleanup");
        assert_eq!(
            after_pending(&c, cleanup.start, cleanup.end)
                .map(|(p, _)| c.steps[p].task.name.as_str()),
            Some("second cleanup"),
            "a pending failure still finishes the section"
        );
        assert_eq!(
            after_failure(&c, cleanup.start),
            None,
            "a failure raised here ends the section instead"
        );
    }

    /// A cleanup written as a block runs to the end of the section, and a failure inside that
    /// block ends the cleanup where the reference ends it.
    ///
    /// Measured on ansible-core 2.19.12, on this shape: both cleanup tasks run and the recap
    /// reads `ok=2 changed=2 failed=1`; with the first of them failing instead, neither the
    /// second nor the step behind the nested block runs and the recap reads `failed=2`.
    ///
    /// What would make this red: reading the section a host is draining off the step it is on.
    /// A cleanup task inside a nested block carries that block's `Body`, so the host would take
    /// the first such step for the end of the cleanup and leave the play with the rest of it
    /// never run and never reported - a run that exits having done less than the playbook says.
    #[test]
    fn a_cleanup_written_as_a_block_runs_to_the_end_of_the_section() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - block:
            - name: cleanup one
              command: "true"
            - name: cleanup two
              command: "true"
        - name: cleanup last
          command: "true"
"#,
        );
        assert_eq!(
            walk_failure(&c, 0),
            ["cleanup one", "cleanup two", "cleanup last"]
        );
        assert_eq!(
            after_failure(&c, 1),
            None,
            "a failure inside the nested cleanup ends the section, measured"
        );
    }

    /// A nested cleanup that has an `always` of its own runs it, and the section holding it
    /// still finishes.
    ///
    /// Measured on ansible-core 2.19.12 on this shape: `ok=3 changed=3 failed=1`.
    #[test]
    fn a_cleanup_block_runs_its_own_always_and_the_section_goes_on() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - block:
            - name: cleanup one
              command: "true"
          always:
            - name: cleanup inner cleanup
              command: "true"
        - name: cleanup last
          command: "true"
"#,
        );
        assert_eq!(
            walk_failure(&c, 0),
            ["cleanup one", "cleanup inner cleanup", "cleanup last"]
        );
    }

    /// The same on the failure path: a cleanup that opens on a block with an empty `block:` list
    /// opens on that block's rescue, and a host walking out through the `always` sections has
    /// failed somewhere else entirely.
    ///
    /// What would make this red: entering an `always` at its first index rather than at its
    /// first step, which runs a recovery section as cleanup.
    #[test]
    fn a_cleanup_does_not_start_in_a_rescue_either() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - block: []
          rescue:
            - name: cleanup recovery
              command: "true"
        - name: cleanup
          command: "true"
"#,
        );
        assert_eq!(names(&c), ["fails", "cleanup recovery", "cleanup"]);
        assert_eq!(walk_failure(&c, 0), ["cleanup"]);
    }

    /// A block with an empty `block:` list is entered on its rescue, and nothing has failed:
    /// the happy path steps over it, whether the block is the first thing in the play or comes
    /// after another step.
    ///
    /// What would make this red: a walk that only handles leaving a block. Entering one whose
    /// body is empty lands straight on the first step of its rescue, so the recovery path of a
    /// playbook would run as ordinary work.
    #[test]
    fn an_empty_body_is_not_a_way_into_a_rescue() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block: []
      rescue:
        - name: first recovery
          command: "true"
    - name: one
      command: "true"
    - block: []
      rescue:
        - name: second recovery
          command: "true"
      always:
        - name: cleanup
          command: "true"
    - name: two
      command: "true"
"#,
        );
        assert_eq!(
            names(&c),
            ["first recovery", "one", "second recovery", "cleanup", "two"]
        );
        assert_eq!(first(&c), 1, "the play does not start inside a rescue");
        assert_eq!(walk(&c, first(&c)), ["one", "cleanup", "two"]);
    }

    /// Which rescue takes a failure, at every place in a three-level play one can be raised.
    ///
    /// Measured on ansible-core 2.19.12, one playbook per line: a failure in a body goes to that
    /// block's rescue; a failure in a rescue is **not** taken by the block whose rescue it is,
    /// and goes to the grandparent's rescue when there is one (`rescued=2`, exit 0); a failure
    /// in an `always` goes to the enclosing block's rescue (`rescued=1`, exit 0).
    ///
    /// What would make this red: reading the block off the step and ignoring the section, which
    /// sends a failing rescue back into its own rescue - the same steps for ever, and a run that
    /// never returns.
    #[test]
    fn the_rescue_that_takes_a_failure_is_never_the_one_it_was_raised_in() {
        let c = compiled(NESTED);
        let at = |name: &str| index_of(&c, name);
        let target = |name: &str| rescue_target(&c, at(name)).map(|i| c.steps[i].task.name.clone());
        assert_eq!(target("body one").as_deref(), Some("outer rescue"));
        assert_eq!(target("nested body").as_deref(), Some("nested rescue"));
        assert_eq!(
            target("nested rescue").as_deref(),
            Some("outer rescue"),
            "a rescue does not rescue itself; the block around it does"
        );
        assert_eq!(
            target("nested always").as_deref(),
            Some("outer rescue"),
            "measured: a failure in a child's always is taken by the parent's rescue"
        );
        assert_eq!(target("outer rescue"), None);
        assert_eq!(target("outer always"), None);
        assert_eq!(
            target("before"),
            None,
            "a step in no block is rescued by none"
        );
    }

    /// A block that has an `always` and no `rescue` rescues nothing, and the cleanup still runs.
    ///
    /// Measured on ansible-core 2.19.12: `failed=1 rescued=0`, the `always` task runs, and the
    /// step behind the block does not.
    #[test]
    fn a_block_without_a_rescue_cleans_up_and_lets_the_failure_stand() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - name: cleanup
          command: "true"
    - name: after
      command: "true"
"#,
        );
        assert_eq!(rescue_target(&c, 0), None);
        assert_eq!(walk_failure(&c, 0), ["cleanup"]);
    }

    /// A block written inside a rescue is reachable in both directions: its recovery takes a
    /// failure raised in it, and its cleanup runs on the way out.
    ///
    /// Measured on ansible-core 2.19.12: the parent's body fails, the rescue runs, the block in
    /// it fails, its own `always` runs, then the parent's `always`, and the recap reads
    /// `ok=2 failed=1 rescued=1`.
    ///
    /// What would make this red: seeking into those sections as if the host had entered no
    /// rescue. Every step of them sits in a rescue, so all of them would be stepped over - the
    /// cleanup a playbook wrote inside its recovery, skipped in silence.
    #[test]
    fn a_block_inside_a_rescue_keeps_its_own_sections() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: body fails
          command: "true"
      rescue:
        - block:
            - name: in the rescue
              command: "true"
          rescue:
            - name: rescue of the rescue
              command: "true"
          always:
            - name: cleanup in the rescue
              command: "true"
      always:
        - name: outer cleanup
          command: "true"
"#,
        );
        let at = |name: &str| index_of(&c, name);
        assert_eq!(
            rescue_target(&c, 0).map(|i| c.steps[i].task.name.clone()),
            Some("in the rescue".to_string()),
            "the rescue is entered on its first step, not on its first index"
        );
        assert_eq!(
            rescue_target(&c, at("in the rescue")).map(|i| c.steps[i].task.name.clone()),
            Some("rescue of the rescue".to_string())
        );
        assert_eq!(
            walk_failure(&c, at("in the rescue")),
            ["cleanup in the rescue", "outer cleanup"]
        );
    }

    /// The fixture above cannot tell a sought target from a raw index: its rescue's first index
    /// already is its first runnable step. Here it is not - a nested block with an empty
    /// `block:` list opens the rescue on its own un-entered rescue, and `rescue_target` has to
    /// seek past that to reach a step the failure can actually run.
    ///
    /// Measured on ansible-core 2.19.12: a rescue built this way still reads `rescued=1`, exit
    /// 0, with nothing from the nested rescue running.
    ///
    /// What would make this red: `rescue_target` returning `c.blocks[id].rescue.start` unchanged
    /// instead of seeking. That raw index is "rescue recovery" here, a step sitting in the
    /// nested block's own rescue, which this failure never entered - so an unsought return
    /// would name it instead of "rescue cleanup", the sibling behind it.
    #[test]
    fn rescue_target_seeks_past_a_nested_blocks_own_rescue() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      rescue:
        - block: []
          rescue:
            - name: rescue recovery
              command: "true"
        - name: rescue cleanup
          command: "true"
      always:
        - name: outer cleanup
          command: "true"
"#,
        );
        assert_eq!(
            rescue_target(&c, 0).map(|i| c.steps[i].task.name.clone()),
            Some("rescue cleanup".to_string())
        );
    }

    /// Every keyword a block carries reaches the tasks under it, and a task that says something
    /// itself keeps its own answer.
    ///
    /// What would make this red: an inherited keyword dropped in the merge, which runs the task
    /// without the `become`, the `when` or the timeout the operator wrote around it.
    #[test]
    fn a_block_s_keywords_reach_the_tasks_under_it() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - name: outer
      block:
        - name: inherits
          command: "true"
        - name: overrides
          command: "true"
          ignore_errors: false
          become: false
          vars: {shared: task}
          when: inner
      when: outer
      become: true
      become_user: deploy
      ignore_errors: true
      timeout: 7
      vars: {shared: block, only: block}
"#,
        );
        let inherits = &c.steps[0].task;
        assert_eq!(inherits.when, ["outer"]);
        assert_eq!(inherits.r#become, Some(true));
        assert_eq!(inherits.become_user.as_deref(), Some("deploy"));
        assert!(inherits.ignores_errors());
        assert_eq!(inherits.timeout, Some(7));
        assert_eq!(inherits.vars["shared"], serde_json::json!("block"));
        assert_eq!(inherits.vars["only"], serde_json::json!("block"));

        let overrides = &c.steps[1].task;
        assert_eq!(
            overrides.when,
            ["outer", "inner"],
            "the outer condition is evaluated first, so it is the one a skip reports"
        );
        assert_eq!(overrides.r#become, Some(false));
        assert!(
            !overrides.ignores_errors(),
            "measured: a task's own 'ignore_errors: false' beats the block's 'true'"
        );
        assert_eq!(overrides.vars["shared"], serde_json::json!("task"));
        assert_eq!(overrides.vars["only"], serde_json::json!("block"));
        // The block's name belongs to no step: measured, it is shown nowhere.
        assert_eq!(names(&c), ["inherits", "overrides"]);
    }

    /// A `meta` task compiles into a step of its own, so nothing tries to send it to a host.
    #[test]
    fn meta_compiles_into_a_step_of_its_own() {
        let c = compiled("- hosts: all\n  tasks:\n    - meta: noop\n    - command: \"true\"\n");
        assert_eq!(c.steps[0].kind, StepKind::Meta);
        assert_eq!(meta_action(&c.steps[0].task), "noop");
        assert_eq!(c.steps[1].kind, StepKind::Task);
    }

    /// The measurement playbook the tag algebra was read off, task for task.
    const TAGGED: &str = r#"
- hosts: all
  tasks:
    - name: a
      debug: msg=a
      tags: [x]
    - name: b
      debug: msg=b
      tags: y
    - name: c-always
      debug: msg=c
      tags: always
    - name: d-never
      debug: msg=d
      tags: never
    - block:
        - name: e-in-block
          debug: msg=e
      tags: [blk]
    - name: f-untagged
      debug: msg=f
"#;

    /// Every selection measured against ansible-core 2.19.12 on this exact playbook, each one
    /// asserting the whole list of surviving names rather than the presence of one of them.
    ///
    /// What would make this red: `always` treated as an ordinary tag, which turns `--tags x`
    /// into `[a]` and empties `--tags never`, `--tags untagged` and `--tags blk` of the one
    /// task that has to be in all four; `never` forgotten, which puts `d-never` into the
    /// default list; `untagged` read as an empty set rather than as the reference's
    /// one-element `{untagged}`, which breaks `--tags untagged` and `--skip-tags untagged` in
    /// opposite directions; or `--skip-tags all` reading as "skip everything", which drops the
    /// `always` task the reference keeps.
    #[test]
    fn every_measured_tag_selection_keeps_the_tasks_the_reference_keeps() {
        let cases: &[(&[&str], &[&str], &[&str])] = &[
            (
                &[],
                &[],
                &["a", "b", "c-always", "e-in-block", "f-untagged"],
            ),
            (
                &["all"],
                &[],
                &["a", "b", "c-always", "e-in-block", "f-untagged"],
            ),
            (&["x"], &[], &["a", "c-always"]),
            (&["blk"], &[], &["c-always", "e-in-block"]),
            (&["never"], &[], &["c-always", "d-never"]),
            (&["untagged"], &[], &["c-always", "f-untagged"]),
            (&["tagged"], &[], &["a", "b", "c-always", "e-in-block"]),
            (&["nosuchtag"], &[], &["c-always"]),
            (&[], &["y"], &["a", "c-always", "e-in-block", "f-untagged"]),
            (&[], &["always"], &["a", "b", "e-in-block", "f-untagged"]),
            (
                &[],
                &["never"],
                &["a", "b", "c-always", "e-in-block", "f-untagged"],
            ),
            (&[], &["untagged"], &["a", "b", "c-always", "e-in-block"]),
            (&[], &["tagged"], &["f-untagged"]),
            (&[], &["all"], &["c-always"]),
            (&["x"], &["x"], &["c-always"]),
        ];
        for (run, skip, want) in cases {
            let c = selected(TAGGED, &selection(run, skip));
            assert_eq!(names(&c), *want, "--tags {run:?} --skip-tags {skip:?}");
        }
    }

    /// A block's tags reach the tasks under it and join whatever they carry themselves, at any
    /// depth, and the play's own tags reach all of them.
    ///
    /// What would make this red: the merge overwriting instead of joining, which loses either
    /// the inner tag or the outer one and makes one of the two `--tags` below select nothing.
    #[test]
    fn tags_accumulate_from_the_play_down_through_nested_blocks() {
        let text = r#"
- hosts: all
  tags: [play]
  tasks:
    - block:
        - block:
            - name: deep
              debug: msg=d
              tags: [own]
          tags: [inner]
      tags: [outer]
"#;
        let c = compiled(text);
        assert_eq!(c.steps[0].task.tags, ["inner", "outer", "own", "play"]);
        for tag in ["play", "outer", "inner", "own"] {
            assert_eq!(
                names(&selected(text, &selection(&[tag], &[]))),
                ["deep"],
                "--tags {tag}"
            );
            assert!(
                names(&selected(text, &selection(&[], &[tag]))).is_empty(),
                "--skip-tags {tag}"
            );
        }
    }

    /// A block whose tasks the selection all drops leaves a span with nothing in it, and the
    /// spans of everything around it still cover exactly the steps they hold.
    ///
    /// What would make this red: filtering the finished list instead of never pushing the
    /// dropped steps. Every span after the first dropped step would then be off by one, which
    /// sends a host into a section it never entered.
    #[test]
    fn filtering_leaves_every_span_covering_its_own_steps() {
        let c = selected(
            r#"
- hosts: all
  tasks:
    - name: kept before
      debug: msg=a
      tags: [keep]
    - block:
        - name: dropped
          debug: msg=b
      always:
        - name: kept cleanup
          debug: msg=c
          tags: [keep]
      tags: [drop]
    - name: kept after
      debug: msg=d
      tags: [keep]
"#,
            &selection(&["keep"], &[]),
        );
        assert_eq!(names(&c), ["kept before", "kept cleanup", "kept after"]);
        let span = &c.blocks[0];
        assert_eq!(span.body, 1..1, "the body lost its only task");
        assert_eq!(span.rescue, 1..1);
        assert_eq!(span.always, 1..2);
        assert_eq!(walk(&c, 0), ["kept before", "kept cleanup", "kept after"]);
    }

    /// Inserting steps one past the last step of a block's body grows that body and shifts the
    /// two sections behind it, and inserting past the block moves nothing.
    ///
    /// What would make this red: `range.end > at` instead of `>=`. The body would not grow, so
    /// the steps spliced in would sit in a range that says they belong to nothing - and the
    /// handler steps a flush point inserts are exactly that shape, since a flush written last in
    /// a body ends it one index before them.
    #[test]
    fn a_splice_grows_the_section_it_lands_at_the_end_of() {
        let mut c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: body one
          command: "true"
        - name: body two
          command: "true"
      rescue:
        - name: recovery
          command: "true"
      always:
        - name: cleanup
          command: "true"
    - name: after
      command: "true"
"#,
        );
        assert_eq!((c.blocks[0].body.start, c.blocks[0].body.end), (0, 2));
        assert_eq!((c.blocks[0].rescue.start, c.blocks[0].rescue.end), (2, 3));
        assert_eq!((c.blocks[0].always.start, c.blocks[0].always.end), (3, 4));
        let step = |name: &str| Step {
            kind: StepKind::Task,
            task: PlayTask {
                name: name.to_string(),
                module: "command".to_string(),
                ..PlayTask::empty()
            },
            block: Some(0),
            section: Section::Body,
            role: None,
        };
        c.splice(2, vec![step("spliced one"), step("spliced two")]);
        assert_eq!(
            names(&c),
            [
                "body one",
                "body two",
                "spliced one",
                "spliced two",
                "recovery",
                "cleanup",
                "after",
            ]
        );
        assert_eq!((c.blocks[0].body.start, c.blocks[0].body.end), (0, 4));
        assert_eq!((c.blocks[0].rescue.start, c.blocks[0].rescue.end), (4, 5));
        assert_eq!((c.blocks[0].always.start, c.blocks[0].always.end), (5, 6));
        // A host that failed in the body is still taken by the block's own rescue and not by
        // the steps that were just put in front of it.
        assert_eq!(
            rescue_target(&c, 0).map(|i| c.steps[i].task.name.clone()),
            Some("recovery".to_string())
        );
        // Past the block, nothing moves. "Past" means past the step behind it: splicing at the
        // index one after the last step of a section is splicing **into** that section, which is
        // what the `>=` above is for and what a flush written last in a body asks for.
        let before = c.blocks[0].clone();
        c.splice(7, vec![step("at the end")]);
        assert_eq!(c.blocks[0].body, before.body);
        assert_eq!(c.blocks[0].rescue, before.rescue);
        assert_eq!(c.blocks[0].always, before.always);
    }

    /// A `notify` reaches the **first** handler of that name and **every** handler listening for
    /// it, in definition order.
    ///
    /// Measured on ansible-core 2.19.12, both halves separately: two handlers written with the
    /// same name run the first alone, and a handler named `x` beside one listening for `x` both
    /// run when `x` is notified.
    ///
    /// What would make this red: every name match returned, which plays a role's handler once
    /// per instance of that role - three times for the role list this repository's own fixture
    /// writes; or `listen` ignored, which loses a handler a playbook asked for.
    #[test]
    fn a_notify_takes_the_first_handler_of_that_name_and_every_listener() {
        let c = compiled(
            r#"
- hosts: all
  handlers:
    - name: twice
      debug: msg=one
    - name: twice
      debug: msg=two
    - name: listening
      debug: msg=three
      listen: twice
  tasks:
    - command: "true"
      notify: twice
"#,
        );
        assert_eq!(c.handlers.len(), 3);
        assert_eq!(resolve_notify(&c, "twice"), [0, 2]);
        assert_eq!(resolve_notify(&c, "listening"), [2]);
        assert!(resolve_notify(&c, "nobody").is_empty());
    }

    /// The three flush points close the three sections, and neither they nor a handler are
    /// listed anywhere.
    ///
    /// Measured on ansible-core 2.19.12: `--list-tasks` on a playbook full of handlers shows the
    /// `meta` the playbook wrote and not one handler, and the implicit flushes are the
    /// reference's own structure rather than anything it prints.
    #[test]
    fn each_section_ends_with_a_flush_point_that_nothing_lists() {
        let c = compiled(
            r#"
- hosts: all
  pre_tasks:
    - name: pre
      command: "true"
  tasks:
    - name: body
      command: "true"
    - meta: flush_handlers
  post_tasks:
    - name: post
      command: "true"
"#,
        );
        let kinds: Vec<StepKind> = c.steps.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [
                StepKind::Task,
                StepKind::Flush { explicit: false },
                StepKind::Task,
                StepKind::Flush { explicit: true },
                StepKind::Flush { explicit: false },
                StepKind::Task,
                StepKind::Flush { explicit: false },
            ]
        );
        let listed: Vec<&str> = (0..c.steps.len())
            .filter(|&i| listed(&c, i))
            .map(|i| c.steps[i].task.name.as_str())
            .collect();
        assert_eq!(listed, ["pre", "body", "meta", "post"]);
    }

    /// A `notify` on a block reaches the tasks under it, and a task that notifies something of
    /// its own keeps both. Measured on ansible-core 2.19.12.
    #[test]
    fn a_block_s_notify_reaches_the_tasks_under_it() {
        let c = compiled(
            r#"
- hosts: all
  handlers:
    - name: h
      debug: msg=h
    - name: own
      debug: msg=own
  tasks:
    - block:
        - name: inherits
          command: "true"
        - name: adds its own
          command: "true"
          notify: own
      notify: h
"#,
        );
        assert_eq!(c.steps[0].task.notify, ["h"]);
        assert_eq!(c.steps[1].task.notify, ["own", "h"]);
    }

    /// The action table is the reference's own list, sorted and free of duplicates.
    #[test]
    fn the_meta_actions_are_sorted_and_free_of_duplicates() {
        let names: Vec<&str> = META_ACTIONS.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted);
    }
}
