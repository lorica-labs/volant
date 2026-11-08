// SPDX-License-Identifier: GPL-3.0-or-later
//! What a play's `hosts` renders against: the play, and no host.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::{Scope, VarStore};
use crate::tags::Tagged;
use crate::template::Vars;

/// The variables a render with no host reads. Owned, because the render borrows all of it at once.
#[derive(Debug)]
pub(crate) struct PlayView {
    pub vars: Tagged,
    /// `groups` and the three host lists.
    pub shared: Arc<Map<String, Value>>,
    /// Which of those list a name from a managed host, once the run has made one.
    pub data_lists: Arc<BTreeSet<String>>,
    /// Every host's view, once the run has a play behind it.
    pub hostvars: Option<Arc<crate::vars::HostvarsMap>>,
}

impl PlayView {
    pub fn vars(&self) -> Vars<'_> {
        self.vars_over(&self.vars)
    }

    /// This view's `groups`, `hostvars` and host lists, over `map`.
    pub fn vars_over<'a>(&'a self, map: &'a Tagged) -> Vars<'a> {
        Vars {
            map,
            tags: Some(map.tags()),
            hostvars: self.hostvars.as_ref(),
            shared: Some(&self.shared),
            data_lists: Some(&self.data_lists),
            facts: None,
            base: None,
        }
    }
}

impl VarStore {
    /// The view the reference builds with a play and no host, which is where it renders a play's
    /// `hosts`. Lowest first: a role's defaults, the play's variables, its `vars_files`, what the
    /// roles export, the extra variables, then the magic variables that need no host.
    ///
    /// A host's inventory variables, its `group_vars`, its facts and its registered results are
    /// not in it, and neither is `inventory_hostname`. `scope` names the host lists
    /// `ansible_play_hosts_all` and the others answer with, which are every host the run could
    /// reach while the pattern is not yet known. `hostvars` is there only when `with_hostvars`
    /// says a play has run: the reference builds it when its first play starts.
    pub fn play_view(&mut self, scope: &Scope, with_hostvars: bool) -> PlayView {
        let mut vars = Tagged::default();
        vars.extend(&scope.role_defaults);
        vars.extend(&scope.play_vars);
        for file in scope.vars_files.maps() {
            vars.extend(file);
        }
        vars.extend(&scope.role_vars);
        vars.extend(&self.extra);
        vars.extend_untagged(&self.play_magic());
        PlayView {
            vars,
            shared: self.shared_values(scope),
            data_lists: self.play_data_lists(),
            hostvars: with_hostvars.then(|| self.hostvars_base()),
        }
    }

    /// What reading the inventory's names makes data. A host or a group that `add_host` or
    /// `group_by` made while the playbook ran can be named by text a managed host sent back, so
    /// once the run has made one, everything that lists names is data: `groups`, the play's
    /// host lists and `hostvars`. This reads the run's own record of what it added, whatever the
    /// name looks like.
    fn play_data_lists(&self) -> Arc<BTreeSet<String>> {
        if self.added.is_empty() {
            return Arc::default();
        }
        let names = [
            "groups",
            "hostvars",
            "ansible_play_hosts",
            "ansible_play_hosts_all",
            "ansible_play_batch",
            "play_hosts",
        ];
        Arc::new(names.iter().map(|n| (*n).to_string()).collect())
    }
}
