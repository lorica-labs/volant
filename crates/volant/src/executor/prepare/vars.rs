// SPDX-License-Identifier: GPL-3.0-or-later
//! The variables one host runs a task with.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::compile::{IncludeParams, Origin};
use crate::template::Templar;
use crate::vars::{HostVars, Scope, VarStore};

use super::connect::insert_search_path;
use super::plan::PlayPlan;
use crate::executor::coordinator::Progress;

/// The merged, self-resolved variables of a host for one task. `live` is the progress the
/// coordinator last published: its batch list feeds `ansible_play_batch` and its play-wide list
/// `ansible_play_hosts`, the two being the same list until `serial` parts them.
///
/// `role` decides which of the two role layers the step sits on: its own role's, which carry the
/// whole play's exported values with that role's own laid over them, or the play-wide export for
/// a step that belongs to no role. Role parameters are the one layer that does not leave the
/// role - measured, a parameter beats a `set_fact` inside the role and is not defined at all in
/// the play's own tasks afterwards.
#[expect(
    clippy::too_many_arguments,
    reason = "the driver's whole context, read for one host's variables"
)]
pub(in crate::executor) fn host_vars(
    host: &str,
    plan: &PlayPlan,
    task_vars: &Map<String, Value>,
    role: Option<usize>,
    include_params: Option<&IncludeParams>,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
    origin: &Origin,
    playbook_dir: &Path,
) -> HostVars {
    delegate_host_vars(
        host,
        Named::ByTheRun,
        plan,
        task_vars,
        role,
        include_params,
        live,
        templar,
        store,
        origin,
        playbook_dir,
    )
}

/// Where the name of a host view came from.
#[derive(Clone, Copy)]
pub(super) enum Named {
    /// The inventory, or the play's own host list.
    ByTheRun,
    /// A `delegate_to`. `data` says the render read a value that came from a managed host.
    ByDelegate { data: bool },
}

/// `host_vars` for the host a `delegate_to` named. A delegate name is data, and the names derived
/// from it (`inventory_hostname`, `inventory_hostname_short`) and the `hostvars` entry that
/// carries it are never rendered, in three cases. The render read a managed host's value. The name
/// holds a template, wherever it came from: no inventory names a host that way, and the reference
/// reads it as a host name, prints it and fails to connect to it. Or the host is not in the
/// inventory the run started from, where `groups` or a variable can hand out text a managed host
/// returned with nothing in the render to say so.
#[expect(
    clippy::too_many_arguments,
    reason = "the driver's whole context, read for one host's variables"
)]
pub(in crate::executor) fn delegate_host_vars(
    host: &str,
    named: Named,
    plan: &PlayPlan,
    task_vars: &Map<String, Value>,
    role: Option<usize>,
    include_params: Option<&IncludeParams>,
    live: &Progress,
    templar: &Templar,
    store: &Mutex<VarStore>,
    origin: &Origin,
    playbook_dir: &Path,
) -> HostVars {
    let merged = merged_vars(
        host,
        named,
        plan,
        task_vars,
        role,
        include_params,
        live,
        store,
        origin,
        playbook_dir,
    );
    resolved(merged, templar)
}

/// [`delegate_host_vars`] before the variables resolve each other: every layer merged, the names
/// that came from a managed host marked, and the engine's own search path in place. A looped
/// `set_fact` keeps this form, because a variable built from a fact it sets has to render again.
#[expect(
    clippy::too_many_arguments,
    reason = "the driver's whole context, read for one host's variables"
)]
pub(in crate::executor) fn merged_vars(
    host: &str,
    named: Named,
    plan: &PlayPlan,
    task_vars: &Map<String, Value>,
    role: Option<usize>,
    include_params: Option<&IncludeParams>,
    live: &Progress,
    store: &Mutex<VarStore>,
    origin: &Origin,
    playbook_dir: &Path,
) -> HostVars {
    let compiled = plan.steps();
    let role = role
        .and_then(|i| compiled.roles.get(i))
        .unwrap_or(&compiled.exported);
    // What an include handed down joins the role parameters rather than the task variables:
    // measured, it beats a `set_fact` of the same name, which a task's own `vars:` does not.
    let mut role_params = role.params.clone();
    for (key, value) in include_params.iter().flat_map(|p| &p.values) {
        role_params.insert(key.clone(), value.clone());
    }
    let scope = Scope {
        play_vars: plan.play_vars.clone(),
        vars_files: plan.vars_files.get(host).cloned().unwrap_or_default(),
        task_vars: task_vars.clone(),
        role_defaults: role.defaults.clone(),
        role_vars: role.vars.clone(),
        role_params,
        play_hosts: live.play_hosts_left.clone(),
        batch_hosts: live.live_hosts.clone(),
        all_play_hosts: plan.all_play_hosts.clone(),
    };
    let mut vars = {
        let mut store = store.lock().expect("vars lock");
        // The shared views first: a variable of this host's own can name `hostvars` or `groups`,
        // and `resolve_vars` below has to be able to answer it.
        let hostvars = store.hostvars_shared(host);
        let mut untrusted = store.untrusted_of(host, &scope);
        // What an include handed down is already rendered, so its provenance cannot be read off
        // the store: it travelled with the values.
        untrusted.extend(
            include_params
                .iter()
                .flat_map(|p| p.untrusted.iter().cloned()),
        );
        let mut untrusted_hosts = store.untrusted_hosts();
        let data = Templar::is_template(host)
            || match named {
                Named::ByTheRun => false,
                Named::ByDelegate { data } => data || store.is_runtime_host(host),
            };
        if data {
            untrusted.extend(["inventory_hostname", "inventory_hostname_short"].map(String::from));
            let mut hosts = (*untrusted_hosts).clone();
            hosts.insert(host.to_string());
            untrusted_hosts = Arc::new(hosts);
        }
        let layers = store.layered_for_host(host, &scope);
        HostVars {
            map: layers.map,
            base: Arc::default(),
            hostvars,
            shared: layers.shared,
            facts: layers.facts,
            untrusted,
            untrusted_hosts,
        }
    };
    // Before anything of the task's own `vars:` resolves, never after: a lookup one of them
    // calls (`first_found`, `template`) reads these two names to find its file, and a pass that
    // resolves the task's variables without them first is a pass that lookup cannot complete on.
    insert_search_path(&mut vars, origin, playbook_dir);
    vars
}

/// The variables of [`merged_vars`] resolved against each other.
pub(in crate::executor) fn resolved(mut vars: HostVars, templar: &Templar) -> HostVars {
    // The merged map carries this host's facts, so the names that came from a managed host
    // travel into the resolution below and are left there exactly as they arrived. The set comes
    // back wider than it went in: a `vars:` of the play, of a block or of the task itself built
    // from a registered value is data too, and this resolution is the only place that can tell.
    let (map, untrusted) = templar.resolve_vars_tainted(&vars);
    vars.map = map;
    vars.untrusted = untrusted;
    vars
}
