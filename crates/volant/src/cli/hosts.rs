// SPDX-License-Identifier: GPL-3.0-or-later

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use super::args::PlaybookArgs;
use crate::bridge::Bridge;
use crate::compile::Compiled;
use crate::compile::TagSelection;
use crate::config::Config;
use crate::executor::{self, HostsContext};
use crate::inventory::{Host, Inventory, Source};
use crate::listing::Listing;
use crate::playbook;
use crate::render::Renderer;
use crate::stats::Refusal;
use crate::template::Templar;
use crate::vars::VarStore;

/// The tags the run selects, from the configuration file, the environment and the command line.
///
/// Measured on ansible-core 2.19.12: `--tags` and `--skip-tags` **append** to the list
/// `[tags] run` or `ANSIBLE_RUN_TAGS` set, rather than replacing it, because the option's own
/// default is that list and `append` adds to a default. So `[tags] run = x` with `--tags y`
/// selects `x` and `y`. A value is split on commas and trimmed wherever it came from, so
/// `--tags x,y`, `--tags 'x, y'` and `--tags x --tags y` are the same three-word request.
///
/// `--list-tags` with nothing asked for is the one place the reference widens the selection
/// instead of narrowing it: measured, it lists `never` tags too, because the point of the
/// command is to show every tag a playbook has. A `--tags` or a `--skip-tags` from any source
/// takes that back.
pub(super) fn tag_selection(args: &PlaybookArgs, config: &Config, mode: Listing) -> TagSelection {
    let joined = |from_config: &[String], from_cli: &[String]| -> Vec<String> {
        let mut tags = from_config.to_vec();
        tags.extend(
            from_cli
                .iter()
                .flat_map(|value| crate::config::tag_list(value)),
        );
        tags
    };
    let run = joined(&config.tags_run, &args.tags);
    let skip = joined(&config.tags_skip, &args.skip_tags);
    if run.is_empty() && skip.is_empty() && mode.tags {
        return TagSelection::new(vec!["never".to_string(), "all".to_string()], Vec::new());
    }
    TagSelection::new(run, skip)
}

/// The host names `--limit` narrows the whole run to, or `None` when it was not given.
///
/// Exit 1 when it leaves nothing, which is the reference's own code, measured - and measured
/// again for a listing: `--list-hosts --limit nosuch` exits 1 there too, printing nothing.
pub(super) fn resolve_limit(
    args: &PlaybookArgs,
    inventory: &Inventory,
    out: &mut Renderer,
) -> anyhow::Result<Option<HashSet<String>>> {
    let names = limit_names(args, inventory, out)?;
    if names.as_ref().is_some_and(HashSet::is_empty) {
        anyhow::bail!(
            "Specified inventory, host pattern and/or --limit leaves us with no hosts to target."
        );
    }
    Ok(names)
}

/// What `--limit` names, with the warnings for what it cannot match, and no refusal for a set
/// that comes out empty: the ad hoc command decides that itself.
pub(super) fn limit_names(
    args: &PlaybookArgs,
    inventory: &Inventory,
    out: &mut Renderer,
) -> anyhow::Result<Option<HashSet<String>>> {
    let Some(pattern) = &args.hosts.limit else {
        return Ok(None);
    };
    let resolution = inventory.resolve(pattern);
    if let Some(bad) = &resolution.invalid {
        anyhow::bail!("Invalid host list pattern: {bad}");
    }
    // A `--limit` naming a host that is not there is a warning and not a refusal, the way it is
    // in a play's own `hosts`: measured, `-l web1,web-typo` warns `Could not match supplied host
    // pattern, ignoring: web-typo` and then runs on `web1`. Losing it meant a mistyped name
    // silently narrowed the run to nothing it was asked about.
    for unmatched in &resolution.unmatched {
        out.warning(
            &format!("Could not match supplied host pattern, ignoring: {unmatched}"),
            false,
        );
    }
    Ok(Some(resolution.hosts.into_iter().map(|h| h.name).collect()))
}

/// The play with its `hosts` rendered, for a play whose `hosts` is a template, against the
/// run's own variables.
pub(super) fn rendered<'a>(
    play: &'a playbook::Play,
    compiled: &Compiled,
    state: &executor::RunState,
    inventory: &Inventory,
    limit: Option<&HashSet<String>>,
) -> anyhow::Result<Cow<'a, playbook::Play>> {
    let (templar, vars) = (&state.templar, &state.vars);
    render_with(
        play,
        compiled,
        templar,
        vars,
        inventory,
        limit,
        state.played,
    )
}

fn render_with<'a>(
    play: &'a playbook::Play,
    compiled: &Compiled,
    templar: &Templar,
    store: &Mutex<VarStore>,
    inventory: &Inventory,
    limit: Option<&HashSet<String>>,
    with_hostvars: bool,
) -> anyhow::Result<Cow<'a, playbook::Play>> {
    if !executor::is_templated(play) {
        return Ok(Cow::Borrowed(play));
    }
    let all_hosts: Vec<String> = inventory
        .all_host_names()
        .into_iter()
        .filter(|name| limit.is_none_or(|allowed| allowed.contains(name)))
        .collect();
    executor::render_hosts(
        play,
        &HostsContext {
            templar,
            vars: store,
            compiled,
            all_hosts: &all_hosts,
            with_hostvars,
        },
    )
}

/// What a listing renders a play's `hosts` against: a store over the inventory and the extra
/// variables, built when a play needs it and not otherwise, so a listing of plain patterns reads
/// no `group_vars` it did not read before.
pub(super) struct ListingHosts<'a> {
    args: &'a PlaybookArgs,
    sources: &'a [Source],
    bridge: &'a Arc<Bridge>,
    selection: &'a TagSelection,
    limit: Option<&'a HashSet<String>>,
    store: Option<Mutex<VarStore>>,
}

impl<'a> ListingHosts<'a> {
    pub fn new(
        args: &'a PlaybookArgs,
        sources: &'a [Source],
        bridge: &'a Arc<Bridge>,
        selection: &'a TagSelection,
        limit: Option<&'a HashSet<String>>,
    ) -> Self {
        Self {
            args,
            sources,
            bridge,
            selection,
            limit,
            store: None,
        }
    }

    /// The play with its `hosts` rendered, as the run renders it, with no play behind it.
    pub fn render<'p>(
        &mut self,
        play: &'p playbook::Play,
        compiled: &Compiled,
        inventory: &Inventory,
    ) -> anyhow::Result<Cow<'p, playbook::Play>> {
        if !executor::is_templated(play) {
            return Ok(Cow::Borrowed(play));
        }
        if self.store.is_none() {
            let extra =
                crate::vars::parse_extra_vars(&self.args.extra_vars, &std::env::current_dir()?)?;
            let mut store = VarStore::with_sources(inventory, self.sources, &play.dir, extra)?;
            let (run, skip) = self.selection.lists();
            store.set_tags(run, skip);
            store.set_modes(self.args.check, self.args.diff);
            self.store = Some(Mutex::new(store));
        }
        let store = self.store.as_ref().expect("the store is built");
        store.lock().expect("vars lock").rebase(&play.dir)?;
        let templar = Templar::with_bridge(play.dir.clone(), Some(Arc::clone(self.bridge)));
        render_with(
            play, compiled, &templar, store, inventory, self.limit, false,
        )
    }
}

/// The hosts one play would run on: its own pattern resolved, narrowed by `--limit`. One
/// function for the run and for `--list-hosts`, so the listing cannot show a host list the run
/// would not use.
pub(super) fn play_hosts(
    inventory: &Inventory,
    play: &playbook::Play,
    limit: Option<&HashSet<String>>,
    out: &mut Renderer,
    warned: &mut HashSet<String>,
    ordered: bool,
) -> anyhow::Result<Vec<Host>> {
    let resolution = inventory.resolve(&play.hosts);
    if let Some(bad) = &resolution.invalid {
        anyhow::bail!("Invalid host list pattern: {bad}");
    }
    for warning in &resolution.warnings {
        if warned.insert(warning.clone()) {
            out.warning(warning, false);
        }
    }
    for pattern in &resolution.unmatched {
        out.warning(
            &format!("Could not match supplied host pattern, ignoring: {pattern}"),
            false,
        );
    }
    let mut hosts: Vec<Host> = match limit {
        Some(allowed) => resolution
            .hosts
            .into_iter()
            .filter(|h| allowed.contains(&h.name))
            .collect(),
        None => resolution.hosts,
    };
    if ordered {
        order_hosts(&mut hosts, play.order.as_deref())?;
    }
    Ok(hosts)
}

/// The play's `order`: the order its hosts run in, `inventory` when it says nothing. Exit 5 for
/// a value the reference does not know, which is its own code, measured, and it speaks before
/// the play's banner.
pub(super) fn order_hosts(hosts: &mut [Host], order: Option<&str>) -> anyhow::Result<()> {
    match order.unwrap_or("inventory") {
        "inventory" => {}
        "reverse_inventory" => hosts.reverse(),
        "sorted" => hosts.sort_by(|a, b| a.name.cmp(&b.name)),
        "reverse_sorted" => hosts.sort_by(|a, b| b.name.cmp(&a.name)),
        "shuffle" => shuffle(hosts),
        other => {
            return Err(Refusal::at(
                5,
                format!("Invalid 'order' specified for inventory hosts: {other}"),
            ));
        }
    }
    Ok(())
}

/// A Fisher-Yates shuffle, unseeded like the reference's `random.shuffle`: the seed is the one
/// `RandomState` draws from the system for every hash map of the process.
pub(super) fn shuffle<T>(items: &mut [T]) {
    use std::hash::{BuildHasher, Hasher};
    let state = std::collections::hash_map::RandomState::new();
    for i in (1..items.len()).rev() {
        let mut hasher = state.build_hasher();
        hasher.write_usize(i);
        items.swap(i, (hasher.finish() % (i as u64 + 1)) as usize);
    }
}
