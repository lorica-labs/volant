// SPDX-License-Identifier: GPL-3.0-or-later
//! Dynamic includes and imports resolved while a host runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::playbook::{Apply, PlayTask, TaskFileRefused};
use crate::roles::{RoleEntry, RoleVars};

use super::builder::{Builder, sorted};
use super::flatten::unknown_options;
use super::inherit::{extend, lay_under, merge};
use super::model::{BlockSpan, Compiled, IncludeParams, Step};
/// What one host asked for at an include step, for one item of its loop.
#[derive(Debug, Clone)]
pub(crate) struct IncludeRequest {
    pub target: IncludeTarget,
    /// What the `included:` line names: the absolute path of the file, or the role's name.
    pub what: String,
    /// The variables the statement hands to everything it brings in: its own `vars:`, the loop
    /// variable of the item that asked for it and `ansible_loop_var` beside it.
    pub vars: IncludeParams,
    /// The item's display label, which the `included:` line carries as `=> (item=...)`.
    pub label: Option<String>,
    /// For a role, what makes two statements the same role instance for a host that has already
    /// run one: see [`role_identity`].
    pub role_id: Option<String>,
}

impl IncludeRequest {
    /// What two hosts have to agree on to be named on one `included:` line.
    ///
    /// Measured on ansible-core 2.19.12: two hosts including the same file share a line
    /// (`included: /abs/inc-a.yml for h1, h2`), two hosts whose rendered name differs get one
    /// line each, and a loop gets one line per item with its own `=> (item=)`. The variables are
    /// part of it because two hosts handing the same file different values are two different
    /// expansions, whatever the line would read.
    pub fn key(&self) -> String {
        serde_json::json!({
            "what": &self.what,
            "vars": sorted(&self.vars.values),
            "untrusted": &self.vars.untrusted,
            "label": &self.label,
            "role": &self.role_id,
            "flags": match &self.target {
                IncludeTarget::Role(entry) => [entry.public, entry.unique, entry.validate],
                IncludeTarget::File(_) => [false; 3],
            },
        })
        .to_string()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum IncludeTarget {
    /// A tasks file, already resolved to an absolute path that exists.
    File(PathBuf),
    /// A role whose directory the driver has already found, so the message a role nobody can find
    /// gets is the one the reference gives it rather than the compiler's.
    Role(Box<RoleEntry>),
}

/// The steps, blocks, roles and handlers one include request brings in, compiled against the
/// play's own keywords and the run's tag selection.
///
/// The indices inside it are its own: a block it lays out is numbered from zero and a role it
/// reads is numbered from zero, because nothing here knows where the result will land. [`graft`]
/// is what rebases them onto the running play, and it is the only caller.
///
/// What comes down from the statement is measured, and it is less than the plan assumed: only its
/// `vars:`. The tags written on the statement stop there (`--tags inc` runs the include and not
/// the tasks it brought in), `when` is evaluated for the statement alone, and `become` and
/// `environment` are not even accepted on one - ansible-core 2.19.12 refuses them at load time
/// with `'environment' is not a valid attribute for a TaskInclude`. What every layer **around**
/// the statement says still reaches them ([`super::model::Origin::inherited`]), measured for tags: `--tags <a
/// play tag>`, `--tags <the role entry's tag>` and `--tags <a tag of the block around the
/// statement>` each run a task an `include_tasks` brought in.
///
/// **An `Err` from here is one host's task failure**, rescuable and `ignore_errors`-able, because
/// the single caller turns it into that. The pre-flight over the spliced content at the end relies
/// on it: a refusal has to read as the statement failing for this host, not as the run stopping.
/// A new caller that is not a run-time splice would break that and would also pull the pre-flight
/// into a path it must stay out of - listing and syntax-checking never refuse, so a playbook can
/// be read ahead of the release that runs it.
pub(crate) fn expand_include(
    base: &Compiled,
    parent: &Step,
    request: &IncludeRequest,
) -> anyhow::Result<Compiled> {
    let depth = parent.origin.depth + 1;
    // The layers around the statement, the play's among them, without the statement's own
    // keywords. What the statement hands down does not travel in the merge: it is a variable
    // layer of its own, laid on by `graft` at the precedence measured for it, and putting it here
    // as well would put it under the host's facts instead of over them.
    let mut inherited = parent
        .origin
        .inherited
        .as_deref()
        .unwrap_or(&base.inherited)
        .clone();
    // `apply:` is a block the reference puts between the statement and what it brings in, so its
    // keywords are one more layer around those tasks and the statement's own are still not.
    match &parent.task.apply {
        Some(Apply::Keywords(keywords)) => inherited = merge(&inherited, keywords),
        // Raised here and not at load: measured, the statement's banner shows first.
        Some(Apply::Refused(sentence)) => {
            return Err(anyhow::Error::new(TaskFileRefused(
                sentence.to_string(),
                false,
            )));
        }
        None => {}
    }
    let mut builder = Builder {
        steps: Vec::new(),
        blocks: Vec::new(),
        roles: Vec::new(),
        handlers: Vec::new(),
        search: &base.search,
        file_dir: parent.origin.file_dir.clone(),
        role_dir: parent.origin.role_dir.clone(),
        parent_roles: parent.origin.parent_roles.clone(),
        depth: 0,
        include_depth: depth,
        selection: &base.selection,
        exporting: true,
        imported: Vec::new(),
        static_vars: Map::new(),
        role_chain: Vec::new(),
        outer_role: None,
        exports: Vec::new(),
        played: Vec::new(),
    };
    // `block` and `role` are seeded empty and rebased by `graft`: a sub-compilation that mixed
    // the running play's absolute indices with its own relative ones would be a list where the
    // reader cannot tell which it is holding, and a block index off by the wrong base is how a
    // host walks into a section it never entered.
    match &request.target {
        IncludeTarget::File(path) => {
            let tasks = crate::playbook::parse_tasks_file(path)?;
            builder.file_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            builder.outer_role = parent
                .role
                .and_then(|i| base.roles.get(i))
                .map(RoleVars::full_name);
            builder.items(&tasks, &inherited, None, parent.section, None)?;
        }
        IncludeTarget::Role(entry) => {
            // A fresh `seen` list, as for `import_role`: measured, an `include_role` runs its role
            // again even when the play's `roles:` list has already run it. Its own dependencies
            // still deduplicate against each other. The role the statement is written in heads the
            // roles the included one is searched after, as `Task.get_search_path` walks them.
            builder.parent_roles = parent
                .origin
                .role_dir
                .iter()
                .chain(&parent.origin.parent_roles)
                .cloned()
                .collect();
            if let Some(role) = parent.role.and_then(|i| base.roles.get(i)) {
                builder
                    .role_chain
                    .push(super::role_entry::ChainLink::of(role));
            }
            builder.role(
                entry,
                &inherited,
                &mut Vec::new(),
                None,
                parent.section,
                crate::roles::RoleKind::Include,
            )?;
        }
    }
    let mut steps = builder.steps;
    let mut handlers = builder.handlers;
    // Warned about by the compilation of the play; an include read while it runs has no
    // place to say it.
    let _ = crate::collection::resolve_all(&mut steps, &mut handlers, &base.search);
    let expanded = Compiled {
        steps,
        blocks: builder.blocks,
        roles: builder.roles,
        exported: RoleVars::default(),
        handlers,
        inherited,
        search: base.search.clone(),
        selection: base.selection.clone(),
        // An expansion is spliced into a play that has already gathered, so it never carries a
        // gather step of its own.
        gathers: false,
        idle_gather: None,
        crosses: Vec::new(),
        resolved: Arc::clone(&base.resolved),
        warnings: Vec::new(),
        start_at: None,
    };
    // The pre-flight ran once, over the compilation, and none of this was in it. Refused here,
    // before anything is spliced, so what a file this release cannot execute gets is the
    // statement's own failure rather than a keyword read and then dropped.
    crate::preflight::check_spliced(&expanded)?;
    Ok(expanded)
}

/// Puts the expansions of one include step into the running play, at `at`, and answers with the
/// steps that went in.
///
/// Each group brings its own numbering, so three things are rebased: a step's `block` and `role`,
/// and a span's `parent` and its three ranges. `None` means "whatever the statement itself sat
/// in", which is what puts an included task in the block and the section the include was written
/// in - measured, an `include_tasks` in a block's body whose included task fails is taken by that
/// block's `rescue`.
///
/// The spans are appended **after** [`Compiled::splice`] has run, with their indices already
/// absolute: `splice` grows the ranges that were there before it, and a range it has never seen
/// must not be grown twice.
/// One expansion on its way into the running play.
pub(crate) struct Grafted {
    pub expanded: Compiled,
    /// What the statement handed down, for every step of this expansion.
    pub params: IncludeParams,
    /// The hosts whose own statement asked for it.
    pub hosts: Arc<[String]>,
}

pub(crate) fn graft(
    base: &mut Compiled,
    at: usize,
    parent: &Step,
    groups: Vec<Grafted>,
) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut spans: Vec<BlockSpan> = Vec::new();
    for Grafted {
        expanded: mut sub,
        params,
        hosts,
    } in groups
    {
        let block_offset = base.blocks.len() + spans.len();
        let role_offset = base.roles.len();
        let step_offset = at + steps.len();
        let params = Arc::new(params);
        for mut step in sub.steps {
            step.block = step.block.map(|b| b + block_offset).or(parent.block);
            step.role = step.role.map(|r| r + role_offset).or(parent.role);
            // An `import_role` of the expansion named its parent role already, nearer than any
            // the statement hands down.
            step.include_params = Some(match &step.include_params {
                Some(own) => Arc::new(own.under(&params)),
                None => Arc::clone(&params),
            });
            // A dependency the coordinator narrowed to the hosts that have not run it keeps that.
            step.hosts = Some(step.hosts.take().unwrap_or_else(|| Arc::clone(&hosts)));
            // The tasks a handler's include brought run as handlers of the same flush.
            if let (super::StepKind::Handler(i), super::StepKind::Task) = (parent.kind, step.kind) {
                step.kind = super::StepKind::Handler(i);
            }
            steps.push(step);
        }
        for mut span in sub.blocks {
            for range in [&mut span.body, &mut span.rescue, &mut span.always] {
                range.start += step_offset;
                range.end += step_offset;
            }
            span.parent = span.parent.map(|p| p + block_offset).or(parent.block);
            spans.push(span);
        }
        publish_roles(base, &mut sub.roles);
        base.roles.extend(sub.roles);
        // Behind the play's own, measured: a play handler and an included role's handler notified
        // in that order run in that order. Appending is also what keeps every index a host is
        // already carrying in its notification list pointing at the handler it named. The role a
        // handler belongs to was numbered from zero with the rest of the expansion.
        base.handlers
            .extend(sub.handlers.into_iter().map(|mut handler| {
                handler.role = handler.role.map(|r| r + role_offset);
                handler
            }));
    }
    base.splice(at, steps.clone());
    base.blocks.extend(spans);
    steps
}

/// Settles the variables of the roles one expansion brought in.
///
/// Measured on ansible-core 2.19.12: a role an `include_role` reads sees what the public roles of
/// the play lend, and a public one adds its own `defaults` and `vars` to that for everything
/// that runs after it, the roles the play has already compiled included. `public: true` does not
/// reach back: a task that ran before the statement read nothing of it. The play's own tasks and
/// the roles of other hosts see it as well, because the reference keeps the list in the play and
/// not in the host.
fn publish_roles(base: &mut Compiled, brought: &mut [RoleVars]) {
    let mut group = (Map::new(), Map::new());
    for role in brought.iter() {
        extend(&mut group.0, &role.defaults);
        extend(&mut group.1, &role.vars);
    }
    let mut published = false;
    for role in brought.iter().filter(|role| role.public) {
        extend(&mut base.exported.defaults, &role.defaults);
        published = true;
    }
    for role in brought.iter().filter(|role| role.public) {
        extend(&mut base.exported.vars, &role.vars);
    }
    if published {
        for role in &mut base.roles {
            lay_under(role, &base.exported, None);
        }
    }
    for role in brought {
        lay_under(role, &base.exported, Some(group.clone()));
    }
}

/// The role one `include_role` task names, read out of its arguments and whatever the item
/// rendered for them.
pub(crate) fn include_role_entry(args: &Map<String, Value>) -> anyhow::Result<RoleEntry> {
    include_role_options(args)?;
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let name = text("name")
        .or_else(|| text("role"))
        .ok_or_else(|| anyhow::anyhow!("'include_role' takes a role name"))?;
    let from = |key: &str, fallback: &str| text(key).unwrap_or(fallback).to_string();
    Ok(RoleEntry {
        // As for `import_role`: everything the statement says is an argument of the statement,
        // and what it hands the role travels in the task's own `vars:`.
        params: Map::new(),
        keywords: PlayTask::empty(),
        // An include keeps the variables of its role to the role unless it says `public: true`,
        // and unlike an import it runs its role again unless it says `allow_duplicates: false`.
        public: role_flag(
            args,
            "public",
            crate::roles::private_role_vars().is_some_and(|private| !private),
        )?,
        validate: role_flag(args, "rolespec_validate", true)?,
        unique: !role_flag(args, "allow_duplicates", true)?,
        ..RoleEntry::named(
            name,
            crate::roles::RoleFrom {
                tasks: from("tasks_from", "main"),
                vars: from("vars_from", "main"),
                defaults: from("defaults_from", "main"),
                handlers: from("handlers_from", "main"),
            },
        )
    })
}

/// What makes two `include_role` statements one role instance, for the host that ran the first
/// when the second says `allow_duplicates: false`.
///
/// Measured on ansible-core 2.19.12: the role, the files it was read from, the `vars:` of the
/// statement and what its loop bound. The same item twice is one instance, two items are two, and
/// so are one item under two names of the loop variable.
pub(crate) fn role_identity(entry: &RoleEntry, vars: &Map<String, Value>) -> String {
    // The format of an `import_role` the play compiled, so an include that says
    // `allow_duplicates: false` is not run again for a host that ran that import: measured on
    // ansible-core 2.19.12 with the `roles` target's `end_role.yml`. A role of `roles:` does not
    // count, measured as well.
    let (name, identity) = super::builder::identity_with(entry, vars);
    format!("include {}", serde_json::json!([name, identity]))
}

/// A boolean option of a role statement: `public`, `allow_duplicates` or `rolespec_validate`.
///
/// Read as the reference reads a boolean keyword, so `"no"` is false and `maybe` is refused in
/// its words.
pub(super) fn role_flag(
    args: &Map<String, Value>,
    key: &str,
    default: bool,
) -> anyhow::Result<bool> {
    let Some(value) = args.get(key) else {
        return Ok(default);
    };
    let flag = match value {
        Value::Number(n) => match n.as_f64() {
            Some(1.0) => Some(true),
            Some(0.0) => Some(false),
            _ => None,
        },
        other => crate::executor::as_bool_value(other),
    };
    flag.ok_or_else(|| {
        anyhow::anyhow!(
            "Error processing keyword '{key}': The value {} could not be converted to 'bool'.",
            crate::playbook::python_repr(value)
        )
    })
}

/// The arguments `include_role` takes. All four options the reference documents beside the file
/// selectors are honoured: `apply` is read with the statement, `public` and `allow_duplicates` by
/// the coordinator that splices the role in, and `rolespec_validate` by the compilation of the
/// role.
pub(super) fn include_role_options(args: &Map<String, Value>) -> anyhow::Result<()> {
    unknown_options(
        "include_role",
        args,
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
            "apply",
        ],
        &[],
    )
}

/// `include_tasks` takes a file name and `apply`, which the compilation of what the statement
/// brings in reads from the task.
pub(super) fn include_options(statement: &str, args: &Map<String, Value>) -> anyhow::Result<()> {
    unknown_options(statement, args, &["file", "_raw_params", "apply"], &[])
}

/// Everything an `include_vars` can be told. A name outside this list is refused before the first
/// connection, for a playbook's own `include_vars` and inside the failing host's own task for one
/// an include brought in, which is where each belongs. What the options mean together is the
/// module's to say when it runs.
pub(crate) fn include_vars_options(args: &Map<String, Value>) -> anyhow::Result<()> {
    const KNOWN: &[&str] = &[
        "_raw_params",
        "file",
        "name",
        "hash_behaviour",
        "dir",
        "depth",
        "extensions",
        "files_matching",
        "ignore_files",
        "ignore_unknown_extensions",
    ];
    unknown_options("include_vars", args, KNOWN, &[])
}
