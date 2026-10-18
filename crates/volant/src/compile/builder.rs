// SPDX-License-Identifier: GPL-3.0-or-later
//! The tree walk that lays a play out as steps.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;
use serde_json::{Map, Value};
use volant_protocol::modules::short_name;

use crate::playbook::{Block, PlayTask, TaskOrBlock};
use crate::roles::{RoleEntry, RoleSearch, RoleVars};
use crate::stats::Refusal;

use super::compiled::{FLUSH_HANDLERS, beside_or_in_role, meta_action};
use super::flatten::{import_role_entry, unknown_options, validation};
use super::include::{include_options, include_role_options, include_vars_options};
use super::inherit::merge;
use super::model::{BlockSpan, Handler, IncludeKind, Origin, Section, Step, StepKind};
use super::tags::TagSelection;
/// How deep roles and imported files may nest before the run is refused: the same ceiling a
/// cycle through an inventory's `:children` is given, for the same reason - a cycle here reads
/// files and recurses on what it finds until the stack is gone.
///
/// One counter for all three recursions - a `meta/main.yml` dependency, an `import_role` inside
/// a role's tasks, an `import_tasks` inside an imported file - because a cycle can run through
/// any mixture of them, and a counter that only one of them increments bounds nothing.
pub(crate) const DEPTH: usize = 32;

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
pub(super) type Seen = Vec<(String, Value)>;

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
pub(super) fn sorted(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut pairs: Vec<(&String, &Value)> = map.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
}

pub(super) struct Builder<'a> {
    pub(super) steps: Vec<Step>,
    pub(super) blocks: Vec<BlockSpan>,
    pub(super) roles: Vec<RoleVars>,
    /// Every role's handlers, in the order the roles run. The play's own are appended behind
    /// them, once, when the walk is over.
    pub(super) handlers: Vec<Handler>,
    pub(super) search: &'a RoleSearch,
    /// The directory relative paths are read against: the playbook's, then a role's `tasks/`
    /// while its tasks are being compiled, then the directory of an imported file while its own
    /// imports are being read. Measured: `import_tasks: sub/extra.yml` inside a role's
    /// `tasks/main.yml` reads `<role>/tasks/sub/extra.yml`, and an `import_tasks` inside **that**
    /// file reads it beside itself.
    pub(super) file_dir: PathBuf,
    /// The root of the role being compiled, if any. Its `tasks/` is the first place an
    /// `import_tasks` written inside a role looks, and its `vars/` the first place an
    /// `include_vars` does.
    pub(super) role_dir: Option<PathBuf>,
    /// The roles whose `meta/main.yml` pulled in the role being compiled, nearest first.
    pub(super) parent_roles: Vec<PathBuf>,
    /// How many roles and imported files are open above whatever is being compiled now.
    pub(super) depth: usize,
    /// How many includes deep the statement that started this compilation was. Zero for a play;
    /// one more than its parent for everything an include splices in.
    pub(super) include_depth: u32,
    /// Which tasks `--tags` and `--skip-tags` leave in. A task the selection drops is never
    /// pushed, rather than pushed and filtered out afterwards: a `retain` over the finished
    /// list would leave every `BlockSpan` pointing at the indices the list used to have, and a
    /// span one step wide of the wrong step is how a host walks into a section it never entered.
    pub(super) selection: &'a TagSelection,
}

impl Builder<'_> {
    /// The one place a step is added, so the tag selection cannot be honoured on one path and
    /// forgotten on another.
    fn push(&mut self, step: Step) {
        if self.selection.selects(&step.task.tags) {
            self.steps.push(step);
        }
    }

    /// Where the steps being laid out now were written, shared by all of them.
    pub(super) fn origin(&self) -> Arc<Origin> {
        Arc::new(Origin {
            file_dir: self.file_dir.clone(),
            role_dir: self.role_dir.clone(),
            parent_roles: self.parent_roles.clone(),
            depth: self.include_depth,
            inherited: None,
        })
    }

    /// One of the three flush points the compiler puts in itself, at the end of a section that
    /// has something in it: `mark` is where the section started, so a section that laid out no
    /// step gets no flush point. Nothing there could have notified anything, and a step every
    /// host has to report through is a barrier.
    ///
    /// Pushed past [`Builder::push`] on purpose: it is the engine's own structure and not
    /// something the playbook wrote, so no `--tags` may drop it. A flush point the selection
    /// removed would leave the handlers a selected task notified with nowhere to run.
    pub(super) fn flush_point(&mut self, mark: usize) {
        if self.steps.len() == mark {
            return;
        }
        self.steps.push(Step {
            kind: StepKind::Flush { explicit: false },
            task: PlayTask::empty(),
            block: None,
            section: Section::Body,
            role: None,
            origin: self.origin(),
            include_params: None,
            hosts: None,
        });
    }

    pub(super) fn items(
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
            // The two dynamic statements become a step of their own rather than a task: nothing
            // runs them on a host, and the arguments they carry are refused here, before the
            // first connection, so an option this release cannot honour cannot be read and
            // dropped while the play is already half applied.
            false => match short_name(&task.module) {
                "include_tasks" => {
                    include_options("include_tasks", &task.args)?;
                    StepKind::Include(IncludeKind::Tasks)
                }
                "include_role" => {
                    include_role_options(&task.args)?;
                    StepKind::Include(IncludeKind::Role)
                }
                // A module like any other, but one whose arguments name work on the controller
                // rather than on a host, so what it can be told is checked with the statements.
                "include_vars" => {
                    include_vars_options(&task.args)?;
                    StepKind::Task
                }
                _ => StepKind::Task,
            },
        };
        let mut origin = self.origin();
        if matches!(kind, StepKind::Include(_)) {
            Arc::make_mut(&mut origin).inherited = Some(Arc::new(inherited.clone()));
        }
        self.push(Step {
            kind,
            task,
            block,
            section,
            role,
            origin,
            include_params: None,
            hosts: None,
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
        let path = beside_or_in_role(&self.file_dir, self.role_dir.as_deref(), name);
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
    pub(super) fn role(
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
        let mut kw = merge(inherited, &entry.keywords);
        // A role does not inherit the play's `collections`: it searches its own `meta/main.yml`
        // list, which may be empty, and a block or task inside it may still write another.
        kw.collections = Some(role.collections.clone());
        self.parent_roles.insert(0, path.clone());
        let dependencies: anyhow::Result<()> = role
            .dependencies
            .iter()
            .try_for_each(|dependency| self.role(dependency, &kw, seen, block, section));
        self.parent_roles.remove(0);
        dependencies?;
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
                origin: self.origin(),
                include_params: None,
                hosts: None,
            });
        }
        let previous = std::mem::replace(&mut self.file_dir, path.join("tasks"));
        let outer_role = self.role_dir.replace(path);
        let result = self.items(&role.tasks, &kw, block, section, Some(index));
        self.file_dir = previous;
        self.role_dir = outer_role;
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
