// SPDX-License-Identifier: GPL-3.0-or-later
//! One role entry read into the play: its dependencies, what it inherits along their chain, its
//! handlers, its argument-spec check and its tasks.

use serde_json::{Map, Value};

use crate::playbook::PlayTask;
use crate::roles::{RoleEntry, RoleKind, RoleVars};
use crate::stats::Refusal;

use super::builder::{Builder, DEPTH, RECURSION, Seen, identity};
use super::flatten::validation;
use super::inherit::{extend, merge};
use super::model::{Handler, Section, Step, StepKind};

/// One role of a dependency chain: its own `defaults` and `vars`, and its parameters over those
/// of the roles above it.
pub(super) struct ChainLink {
    defaults: Map<String, Value>,
    vars: Map<String, Value>,
    params: Map<String, Value>,
}

impl ChainLink {
    /// The role an `include_role` is written in, as the first link of the included role's chain:
    /// what it inherited itself under its own values. Measured on ansible-core 2.19.12 with the
    /// `roles` target's `47023.yml`: a role four `include_role`s down still reads the `vars` and
    /// `defaults` of the first, which is not public.
    pub(super) fn of(role: &RoleVars) -> Self {
        let (own_defaults, own_vars) = role
            .own
            .as_ref()
            .map_or((&role.defaults, &role.vars), |own| {
                (&own.defaults, &own.vars)
            });
        let mut defaults = role.inherited_defaults.clone();
        extend(&mut defaults, own_defaults);
        let mut vars = role.inherited_vars.clone();
        extend(&mut vars, own_vars);
        Self {
            defaults,
            vars,
            params: Map::new(),
        }
    }
}

/// A role's `defaults` and `vars` as it lends them to the roles that depend on it.
pub(super) type Lent = (Map<String, Value>, Map<String, Value>);

impl Builder<'_> {
    /// What a role whose dependencies were just read inherits, see [`RoleVars::inherited_vars`],
    /// and what it lends in turn, kept under its identity `key`.
    fn inherit(&mut self, key: (String, Value), role: &crate::roles::LoadedRole) -> Lent {
        let (mut lent_defaults, mut lent_vars) = (Map::new(), Map::new());
        for dependency in &role.dependencies {
            let key = identity(dependency);
            if let Some((_, (defaults, vars))) = self.exports.iter().rev().find(|(k, _)| *k == key)
            {
                extend(&mut lent_defaults, defaults);
                extend(&mut lent_vars, vars);
            }
        }
        let mut inherited_defaults = lent_defaults.clone();
        let mut inherited_vars = Map::new();
        for link in &self.role_chain {
            extend(&mut inherited_defaults, &link.defaults);
            extend(&mut inherited_vars, &link.vars);
        }
        extend(&mut inherited_vars, &lent_vars);
        extend(&mut lent_defaults, &role.defaults);
        extend(&mut lent_vars, &role.vars);
        self.exports.push((key, (lent_defaults, lent_vars)));
        (inherited_defaults, inherited_vars)
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
        kind: RoleKind,
    ) -> anyhow::Result<()> {
        let path = self.search.locate(&entry.name).map_err(|err| {
            // Measured on ansible-core 2.19.12: `[ERROR]:` and the sentence, at the entry.
            match &entry.keywords.origin {
                Some(origin) => Refusal::plain(
                    1,
                    crate::playbook::Located {
                        message: format!("{err:#}"),
                        origin: origin.clone(),
                    }
                    .text(),
                ),
                None => err,
            }
        })?;
        let role = crate::roles::load(&path, &entry.name, &entry.from)?;
        let key = identity(entry);
        // A role brought in twice is compiled twice: whether a host runs it again is decided
        // while the play runs, once it has completed the first, see `completion`.
        let (dedup, unique) = instance_identity(kind, entry.unique, role.allow_duplicates, &key);
        self.played.push(key.clone());
        seen.push(key.clone());
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
        // What this role lends is settled for it and for the roles it depends on, and restored
        // before its tasks are read: an `import_role` written in them is a statement of its own.
        let outer_exporting = self.exporting;
        // A `roles:` entry cannot say `public`: `private_role_vars` decides for it.
        let public = entry.public
            && !(kind == RoleKind::Entry && crate::roles::private_role_vars() == Some(true));
        self.exporting = outer_exporting && public;
        let exporting = self.exporting;
        let mut params = self
            .role_chain
            .last()
            .map(|link| link.params.clone())
            .unwrap_or_default();
        extend(&mut params, &entry.params);
        self.parent_roles.insert(0, path.clone());
        self.role_chain.push(ChainLink {
            defaults: role.defaults.clone(),
            vars: role.vars.clone(),
            params: params.clone(),
        });
        let dependencies: anyhow::Result<()> =
            role.dependencies.iter().try_for_each(|dependency| {
                self.role(dependency, &kw, seen, block, section, RoleKind::Dependency)
            });
        self.role_chain.pop();
        self.parent_roles.remove(0);
        dependencies?;
        let index = self.roles.len();
        let (inherited_defaults, inherited_vars) = self.inherit(key, &role);
        self.exporting = outer_exporting;
        let (role_name, collection) = crate::roles::identity(&path, &entry.name);
        self.roles.push(RoleVars {
            name: entry.name.clone(),
            role_name,
            collection,
            kind,
            defaults: role.defaults,
            vars: role.vars,
            params,
            inherited_defaults,
            inherited_vars,
            public: exporting,
            dedup,
            unique,
            own: None,
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
        // `rolespec_validate: false` leaves the check out, whatever the role ships.
        if let Some(spec) = role.argument_spec.filter(|_| entry.validate) {
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
}

/// The identity a host that completed this instance records, see [`RoleVars::dedup`], and
/// whether a host that did leaves the instance out. Two kinds of identity, as ansible-core 2.19.12
/// hashes `from_include` into its own: a role an `import_role` brought in is one an `include_role`
/// that says `allow_duplicates: false` does not run again, and a role of `roles:` is not. An
/// `include_role`'s own instance is named by the coordinator, from the statement.
fn instance_identity(
    kind: RoleKind,
    statement_unique: bool,
    allow_duplicates: bool,
    key: &(String, Value),
) -> (Option<String>, bool) {
    let identity = serde_json::json!([&key.0, &key.1]);
    match kind {
        RoleKind::Include => (None, false),
        RoleKind::Import => (Some(format!("include {identity}")), statement_unique),
        _ => (Some(format!("role {identity}")), !allow_duplicates),
    }
}
