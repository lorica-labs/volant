// SPDX-License-Identifier: GPL-3.0-or-later
//! Checking a play's keywords and compiling it.

use std::path::Path;
use std::sync::Arc;

use anyhow::bail;
use serde_json::{Map, Value};

use crate::playbook::{Play, PlayTask};
use crate::roles::{RoleEntry, RoleSearch, RoleVars};
use crate::template::Templar;

use super::builder::{Builder, Seen};
use super::include::refuse_public;
use super::inherit::{extend, layered};
use super::model::{Compiled, Handler, Section, Step, StepKind};
use super::tags::TagSelection;
/// The check the reference inserts in front of a role that ships `meta/argument_specs.yml`.
///
/// Measured: the banner reads `TASK [<role> : Validating arguments against arg spec '<entry>']`,
/// the task runs on the controller, and a failure carries `argument_errors`,
/// `argument_spec_data` and a `validate_args_context` naming the role and its absolute path.
/// A spec carrying a `short_description` puts it after the entry name behind a dash, measured
/// through `--list-tasks`: `Validating arguments against arg spec 'main' - The spec role`.
pub(super) fn validation(
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
pub(super) fn unknown_options(
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
pub(super) fn import_role_entry(task: &PlayTask) -> anyhow::Result<RoleEntry> {
    // Measured: the reference takes these nine and refuses anything else. The last three name
    // work this release does not do - nothing dedups an `import_role`, the argument-spec check
    // has no off switch, and `public` exports nothing of its own - so they are refused rather
    // than accepted and forgotten. `public` is refused by value and not by presence: see
    // `refuse_public`.
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
        &["allow_duplicates", "rolespec_validate"],
    )?;
    refuse_public("import_role", &task.args)?;
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

/// The play's `gather_subset`, `gather_timeout` and `fact_path` as the arguments of its `setup`.
///
/// A `gather_subset` written as one plain string is a one-item list, never split on commas:
/// measured on ansible-core 2.19.12, `"!all,!min,local"` names nothing the module knows and
/// leaves `min`. A template stays as written, and the play start refuses one that renders to a
/// string with a comma in it, which the module would split where the reference does not.
fn gather_args(play: &Play) -> Map<String, Value> {
    let mut args = Map::new();
    if let Some(subset) = &play.gather_subset {
        let subset = match subset {
            Value::String(text) if !Templar::is_template(text) => {
                Value::Array(vec![subset.clone()])
            }
            other => other.clone(),
        };
        args.insert("gather_subset".into(), subset);
    }
    for (name, value) in [
        ("gather_timeout", &play.gather_timeout),
        ("fact_path", &play.fact_path),
    ] {
        if let Some(value) = value {
            args.insert(name.into(), value.clone());
        }
    }
    args
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
#[expect(clippy::too_many_lines)]
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
        role_dir: None,
        depth: 0,
        include_depth: 0,
        selection,
    };
    // The play's own tags are the outermost layer of the merge, so every task under it - in a
    // role, in an imported file, at any block depth - carries them for the selection and for
    // the listing. Measured: `--skip-tags <play tag>` leaves a play with nothing to do.
    // `no_log`, `environment`, `check_mode`, `diff` and `run_once` join the play's tags here: all
    // are task keywords a play may also carry, so putting them in the outermost layer of the
    // merge is what
    // makes a play-wide `environment` reach a task inside a role inside a block, with no second
    // path to keep in step with this one.
    let empty = PlayTask {
        tags: play.tags.clone(),
        no_log: play.no_log,
        environment: play.environment.clone(),
        module_defaults: play.module_defaults.clone(),
        collections: play.collections.clone(),
        check_mode: play.check_mode.clone(),
        diff: play.diff.clone(),
        run_once: play.run_once,
        timeout: play.timeout,
        throttle: play.throttle.clone(),
        debugger: play.debugger.clone(),
        any_errors_fatal: play.any_errors_fatal,
        ignore_errors: play.ignore_errors.clone(),
        ignore_unreachable: play.ignore_unreachable,
        connect: play.connect.clone(),
        ..PlayTask::empty()
    };
    // `gather_facts` is the play's own first task and nothing else: the reference's `setup`
    // module, run through the ordinary Python path, whose `ansible_facts` the driver writes as a
    // managed host's own words. Laid out here, as `steps[0]`, rather than run beside the play:
    // the pre-flight then reads it, the run builds it a payload with every other module the play
    // names, and its result comes back through the one road a result becomes a variable by.
    //
    // `always` is what the reference tags it, so `--tags deploy` still gathers and
    // `--skip-tags always` still does not. The play's own keywords come down through `empty` like
    // any other task's, except `run_once`: facts are the host's own, and gathering them once for
    // the batch would write one host's hostname onto every host of it.
    //
    // Named `ansible.legacy.setup`, as the reference's `gather_facts` plugin runs it: a
    // `library/setup.py` beside the playbook answers the gathering there, measured on
    // ansible-core 2.19.12, where `ansible.builtin.setup` would never find it.
    let gather = PlayTask {
        name: "Gathering Facts".to_string(),
        named: true,
        module: "ansible.legacy.setup".to_string(),
        tags: {
            let mut tags = empty.tags.clone();
            tags.push("always".to_string());
            tags.sort();
            tags.dedup();
            tags
        },
        run_once: None,
        args: gather_args(play),
        origin: play.origin.clone(),
        ..empty.clone()
    };
    let gathers = play.gather_facts && selection.selects(&gather.tags);
    if gathers {
        let origin = builder.origin();
        builder.steps.push(Step {
            kind: StepKind::Task,
            task: gather,
            block: None,
            section: Section::Body,
            role: None,
            origin,
            include_params: None,
            hosts: None,
        });
    }
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
        // The play's `module_defaults` and `collections` reach a handler as they reach a task.
        let mut task = handler.task.clone();
        task.module_defaults = empty
            .module_defaults
            .iter()
            .chain(&handler.task.module_defaults)
            .cloned()
            .collect();
        task.collections = handler
            .task
            .collections
            .clone()
            .or_else(|| empty.collections.clone());
        // And the play's check and diff modes, `--check` and `--diff` among them: a handler
        // that ran for real under `--check` would change the host.
        task.check_mode = task.check_mode.or_else(|| empty.check_mode.clone());
        task.diff = task.diff.or_else(|| empty.diff.clone());
        builder.handlers.push(Handler {
            task,
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
    let mut steps = builder.steps;
    let mut handlers = builder.handlers;
    let warnings = crate::collection::resolve_all(&mut steps, &mut handlers, search);
    Ok(Compiled {
        steps,
        blocks: builder.blocks,
        roles,
        exported,
        handlers,
        inherited: empty,
        search: search.clone(),
        selection: selection.clone(),
        gathers,
        crosses: Vec::new(),
        resolved: Arc::default(),
        warnings,
        start_at: None,
    })
}
