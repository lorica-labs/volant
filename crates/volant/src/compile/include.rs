// SPDX-License-Identifier: GPL-3.0-or-later
//! Dynamic includes and imports resolved while a host runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;
use serde_json::{Map, Value};

use crate::playbook::PlayTask;
use crate::roles::{RoleEntry, RoleVars};

use super::builder::{Builder, sorted};
use super::flatten::unknown_options;
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
    let inherited = parent
        .origin
        .inherited
        .as_deref()
        .unwrap_or(&base.inherited)
        .clone();
    let mut builder = Builder {
        steps: Vec::new(),
        blocks: Vec::new(),
        roles: Vec::new(),
        handlers: Vec::new(),
        search: &base.search,
        file_dir: parent.origin.file_dir.clone(),
        role_dir: parent.origin.role_dir.clone(),
        depth: 0,
        include_depth: depth,
        selection: &base.selection,
    };
    // `block` and `role` are seeded empty and rebased by `graft`: a sub-compilation that mixed
    // the running play's absolute indices with its own relative ones would be a list where the
    // reader cannot tell which it is holding, and a block index off by the wrong base is how a
    // host walks into a section it never entered.
    match &request.target {
        IncludeTarget::File(path) => {
            let tasks = crate::playbook::parse_tasks_file(path)?;
            builder.file_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            builder.items(&tasks, &inherited, None, parent.section, None)?;
        }
        IncludeTarget::Role(entry) => {
            // A fresh `seen` list, as for `import_role`: measured, an `include_role` runs its role
            // again even when the play's `roles:` list has already run it. Its own dependencies
            // still deduplicate against each other.
            builder.role(entry, &inherited, &mut Vec::new(), None, parent.section)?;
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
        expanded: sub,
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
            step.include_params = Some(Arc::clone(&params));
            step.hosts = Some(Arc::clone(&hosts));
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
        base.roles.extend(sub.roles);
        // Behind the play's own, measured: a play handler and an included role's handler notified
        // in that order run in that order. Appending is also what keeps every index a host is
        // already carrying in its notification list pointing at the handler it named.
        base.handlers.extend(sub.handlers);
    }
    base.splice(at, steps.clone());
    base.blocks.extend(spans);
    steps
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
        name: name.to_string(),
        from: crate::roles::RoleFrom {
            tasks: from("tasks_from", "main"),
            vars: from("vars_from", "main"),
            defaults: from("defaults_from", "main"),
            handlers: from("handlers_from", "main"),
        },
        // As for `import_role`: everything the statement says is an argument of the statement,
        // and what it hands the role travels in the task's own `vars:`.
        params: Map::new(),
        keywords: PlayTask::empty(),
    })
}

/// The arguments `include_role` takes, and the three it takes that this release cannot honour.
///
/// `public: true` is measured to export the role's `defaults` and `vars` to everything behind the
/// include, where the default `public: false` keeps them to the role's own tasks - so honouring it
/// would mean changing the play's variable layers while hosts stand at different steps of it.
/// Refused by name, the way `import_role` refuses the same three, rather than accepted and
/// dropped - but by value for `public`, whose false is what this release does. See
/// `refuse_public`.
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
        &["allow_duplicates", "rolespec_validate", "apply"],
    )?;
    refuse_public("include_role", args)
}

/// `public` is the one refusable argument whose **false** is what this release already does.
///
/// The others name work that is missing: nothing dedups a role, the argument-spec check has no
/// off switch, and there is no layer to apply keywords through, so writing any of them at all is
/// asking for something that will not happen. `public: false` asks for the role's `defaults` and
/// `vars` to stay behind the include, which is exactly what happens here - refusing it by
/// presence turns a statement that asks for the engine's own behaviour into an error.
///
/// Anything that is not a spelled-out false is refused, a template included: what `public` will
/// be worth is not known where this runs, and the safe answer for an export this release cannot
/// do is to say so rather than to guess.
pub(super) fn refuse_public(statement: &str, args: &Map<String, Value>) -> anyhow::Result<()> {
    match args.get("public") {
        None => Ok(()),
        Some(value) if crate::executor::as_bool_value(value) == Some(false) => Ok(()),
        Some(_) => bail!("'{statement}' option 'public' is not supported yet"),
    }
}

/// `include_tasks` takes a file name and nothing else. `apply:` puts keywords on everything the
/// statement brings in, which this release has no layer for, so it is refused by its own name.
pub(super) fn include_options(statement: &str, args: &Map<String, Value>) -> anyhow::Result<()> {
    unknown_options(
        statement,
        args,
        &["file", "_raw_params", "apply"],
        &["apply"],
    )
}

/// Everything an `include_vars` can be told, and the seven it can be told that this release
/// cannot honour.
///
/// `dir:` is measured to work in the reference, reading every file of a directory in name order,
/// and the other six narrow which of those files are read or how their values are merged. Refused
/// by their own names rather than read and dropped: a run that silently loaded none of a
/// directory's variables would go on to apply a playbook against defaults nobody wrote.
///
/// Checked here, where the statement becomes a step, rather than in the module: that puts the
/// refusal before the first connection for a playbook's own `include_vars` and inside the failing
/// host's own task for one an include brought in, which is where each belongs.
pub(crate) fn include_vars_options(args: &Map<String, Value>) -> anyhow::Result<()> {
    const SEARCHES: &[&str] = &[
        "dir",
        "depth",
        "extensions",
        "files_matching",
        "ignore_files",
        "ignore_unknown_extensions",
        "hash_behaviour",
    ];
    let mut known = vec!["_raw_params", "file", "name"];
    known.extend_from_slice(SEARCHES);
    unknown_options("include_vars", args, &known, SEARCHES)
}
