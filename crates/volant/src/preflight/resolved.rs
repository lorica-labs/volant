// SPDX-License-Identifier: GPL-3.0-or-later
//! Refusing what the Python helper answered for a module name, and the collections' own actions.

use anyhow::bail;

use super::CODE;
use super::refusals::refuse_async;
use crate::action_plugins;
use crate::playbook::PlayTask;
use crate::stats::Refusal;

/// The reference's sentence for a name nothing answers to.
pub(super) fn unresolved(task: &str, module: &str) -> String {
    format!("task '{task}': {}", unresolved_tail(module))
}

pub(super) fn unresolved_tail(module: &str) -> String {
    format!(
        "couldn't resolve module/action '{module}'. This often indicates a misspelling, missing collection, or incorrect module path."
    )
}

/// Every `(task, module)` of a compiled play whose module only the controller's ansible-core can
/// answer, set aside by [`super::check_task`] for it to resolve.
pub(crate) fn collection_modules(compiled: &crate::compile::Compiled) -> Vec<(String, String)> {
    set_aside(compiled)
        .map(|task| (task.name.clone(), task.module.clone()))
        .collect()
}

pub(super) fn set_aside(compiled: &crate::compile::Compiled) -> impl Iterator<Item = &PlayTask> {
    compiled
        .steps
        .iter()
        .filter(|step| !matches!(step.kind, crate::compile::StepKind::Flush { .. }))
        .map(|step| &step.task)
        .chain(compiled.handlers.iter().map(|handler| &handler.task))
        .filter(|task| crate::python::is_resolved_name(&task.module))
}

/// What the controller's ansible-core made of a collection's module a task names, refused
/// before the first connection unless it is a module the union can hold.
///
/// Volant never installs a collection. A collection's action plugin is not refused: it runs in the
/// Python helper (see `python::types::note_action_plugins`). A missing collection is named, with the command that
/// installs it; a module missing from a collection that is installed is the reference's typo
/// sentence alone.
pub(crate) fn check_resolved(
    task: &str,
    module: &str,
    answer: &crate::python::Resolved,
) -> anyhow::Result<()> {
    match refusal_of(module, answer) {
        None => Ok(()),
        Some(why) => Err(Refusal::at(CODE, format!("task '{task}': {why}"))),
    }
}

/// Why a collection's module the controller resolved cannot run, as the refusal says it after
/// the task's name, or `None` for a module the union can hold. Kept in [`crate::python::Union`]
/// for the names a play does not name itself, so an include reaching one says the same.
pub(crate) fn refusal_of(module: &str, answer: &crate::python::Resolved) -> Option<String> {
    use crate::python::Resolved;
    match answer {
        Resolved::Module { .. } => None,
        // A collection's action plugin runs in the Python helper, as ansible-core's own, unless
        // it needs Windows or a network device's connection.
        Resolved::ActionPlugin { fqcn, .. } => (action_plugins::waits_for_later(module)
            || action_plugins::waits_for_later(fqcn))
        .then(|| {
            format!(
                "module '{fqcn}' needs an action plugin from its collection, which this release does not run"
            )
        }),
        Resolved::Unusable { reason } => Some(format!("module '{module}' cannot run: {reason}")),
        Resolved::Missing { collection: None } => Some(unresolved_tail(module)),
        Resolved::Missing {
            collection: Some(collection),
        } => Some(format!(
            "{} The collection '{collection}' is not installed on the controller: install it with volant runtime install --collections {collection}",
            unresolved_tail(module)
        )),
    }
}

/// A name outside `ansible.builtin` a task gives, on a controller where no ansible-core could be
/// asked what it is: the reference's sentence, then why nothing could look.
pub(crate) fn unresolvable_here(task: &str, module: &str, why: &anyhow::Error) -> anyhow::Error {
    Refusal::at(
        CODE,
        format!(
            "{} Only the controller's ansible-core can resolve a name outside ansible.builtin, and {why:#}",
            unresolved(task, module)
        ),
    )
}

/// A module set aside for the controller, refused unless the union holds it. Run on the plays
/// before the first connection, and on what an include brings in. A word the loader kept in
/// `_raw_params` fails the task when it runs (`raw_params_refusal`).
///
/// The union was built before the first connection, from what the plays and their role files
/// name, and a set-aside module is in it only if the controller's ansible-core resolved it to a
/// module it could build, or an action plugin the helper runs. One that is not was missing, or sits
/// in a file nothing read before the run; the include fails for the host that reached it, before
/// any of the file's tasks run, like any other refusal of an included file.
pub(crate) fn check_built(
    expanded: &crate::compile::Compiled,
    union: Option<&crate::python::Union>,
) -> anyhow::Result<()> {
    check_collection_actions(expanded)?;
    for task in set_aside(expanded).filter(|task| action_plugins::kind(&task.module).is_none()) {
        let (name, module) = (&task.name, &task.module);
        if !union.is_some_and(|u| u.modules.contains_key(crate::python::payload_key(module))) {
            // What the controller made of a name it set aside, as the pre-flight says it for one
            // a play names.
            if let Some(why) = union.and_then(|u| u.refused.get(module)) {
                bail!("task '{name}': {why}");
            }
            bail!(
                "task '{name}': module '{module}' was not resolved to a module before the run, so no payload holds it: its collection is not installed on the controller, no module path holds it, or it is named only in a file nothing read before the first connection"
            );
        }
    }
    Ok(())
}

/// `async` on a collection's action plugin, which the first pass could not see for a plugin until
/// the controller had resolved the name: refused as it is on the builtin ones.
pub(crate) fn check_collection_actions(compiled: &crate::compile::Compiled) -> anyhow::Result<()> {
    set_aside(compiled).try_for_each(refuse_async)
}
