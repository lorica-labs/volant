// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;

use super::builder::PythonBuilder;
use super::types::{Natives, Resolved, Union, is_resolved_name, note_action_plugins};

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
#[cfg(test)]
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
        let mut builder = PythonBuilder::start_installing(!gathering_only(modules))?;
        builder.add_plugin_dirs(plugin_dirs)?;
        Ok(builder)
    };
    union_from(start, modules, named, place.as_ref(), warn)
}

/// `union_for` that also hands back the helper it built, still running, when it built one.
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
        let mut builder = PythonBuilder::start_installing(!gathering_only(modules))?;
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

/// `union_for` with the helper's start and the cache's place handed in, so what happens when
/// there is no helper, or a cache entry, is testable without touching the process environment.
#[cfg(test)]
pub(super) fn union_from(
    start: impl FnOnce() -> anyhow::Result<PythonBuilder>,
    modules: &std::collections::BTreeSet<String>,
    named: &[(String, String)],
    cache: Option<&crate::union_cache::Place>,
    warn: &mut dyn FnMut(String),
) -> anyhow::Result<Option<Union>> {
    union_keeping(start, modules, named, cache, warn, &mut None)
}

/// `union_from` leaving the helper it started in `keep` instead of dropping it.
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

/// Whether gathering facts is all the run asks of ansible-core: such a run never installs
/// Volant's runtime, since `gather_facts: false` is a way out that downloads nothing.
pub(super) fn gathering_only(modules: &std::collections::BTreeSet<String>) -> bool {
    modules.len() == 1 && modules.contains("setup")
}

/// The sentence a run that gathers facts and nothing else gets on top of [`super::builder::refusal_for`].
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
pub(super) fn no_builder(err: &anyhow::Error, modules: &[String]) -> anyhow::Error {
    match modules {
        [only] if only == "setup" => anyhow::anyhow!("{err:#}. {GATHERING_ONLY}"),
        _ => anyhow::anyhow!("{err:#}"),
    }
}
