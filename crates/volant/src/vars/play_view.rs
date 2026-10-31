// SPDX-License-Identifier: GPL-3.0-or-later
//! What a play's `hosts` renders against: the play, and no host.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::load::extend;
use super::{Added, Scope, VarStore};
use crate::template::Vars;

/// The variables a render with no host reads. Owned, because the render borrows all of it at once.
#[derive(Debug)]
pub(crate) struct PlayView {
    pub vars: Map<String, Value>,
    /// `groups` and the three host lists.
    pub shared: Arc<Map<String, Value>>,
    /// Every host's view, once the run has a play behind it.
    pub hostvars: Option<Arc<crate::vars::HostvarsMap>>,
    /// The names that are data. Reading one makes the render's result data, which is never
    /// evaluated again.
    pub untrusted: BTreeSet<String>,
    pub untrusted_hosts: Arc<super::UntrustedHosts>,
}

impl PlayView {
    pub fn vars(&self) -> Vars<'_> {
        self.vars_over(&self.vars, &self.untrusted)
    }

    /// This view's `groups`, `hostvars` and host lists, over `map` and the data names `untrusted`.
    pub fn vars_over<'a>(
        &'a self,
        map: &'a Map<String, Value>,
        untrusted: &'a BTreeSet<String>,
    ) -> Vars<'a> {
        Vars {
            map,
            hostvars: self.hostvars.as_ref(),
            shared: Some(&self.shared),
            facts: None,
            untrusted: Some(untrusted),
            untrusted_hosts: Some(&self.untrusted_hosts),
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
        let mut vars = Map::new();
        extend(&mut vars, &scope.role_defaults);
        extend(&mut vars, &scope.play_vars);
        for file in scope.vars_files.maps() {
            extend(&mut vars, file);
        }
        extend(&mut vars, &scope.role_vars);
        extend(&mut vars, &self.extra);
        extend(&mut vars, &self.play_magic());
        let (untrusted, untrusted_hosts) = self.data_names();
        PlayView {
            vars,
            shared: self.shared_values(scope),
            hostvars: with_hostvars.then(|| self.hostvars_base()),
            untrusted,
            untrusted_hosts,
        }
    }

    /// What reading the inventory's names makes data. A host or a group that `add_host` or
    /// `group_by` made while the playbook ran can be named by text a managed host sent back, so
    /// once the run has made one, everything that lists names is data: `groups`, the play's
    /// host lists and `hostvars`, and the hosts made are the hosts whose entry in `hostvars` is.
    /// This reads the run's own record of what it added, whatever the name looks like.
    fn data_names(&mut self) -> (BTreeSet<String>, Arc<super::UntrustedHosts>) {
        let mut hosts = self.untrusted_hosts();
        if self.added.is_empty() {
            return (BTreeSet::new(), hosts);
        }
        let made = Arc::make_mut(&mut hosts);
        // Only the name of a host the run made is data: its variables are what the playbook's
        // files and the run gave it, each marked as such.
        for change in &self.added {
            if let Added::Host(host, _) | Added::Member(host, _) = change {
                made.entry(host.clone())
                    .or_default()
                    .extend(super::HOST_NAMES.map(String::from));
            }
        }
        let names = [
            "groups",
            "hostvars",
            "ansible_play_hosts",
            "ansible_play_hosts_all",
            "ansible_play_batch",
            "play_hosts",
        ];
        (names.iter().map(|n| (*n).to_string()).collect(), hosts)
    }
}
