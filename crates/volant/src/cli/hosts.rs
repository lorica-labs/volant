// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashSet;

use super::args::PlaybookArgs;
use crate::compile::TagSelection;
use crate::config::Config;
use crate::inventory::{Host, Inventory};
use crate::listing::Listing;
use crate::playbook;
use crate::render::Renderer;
use crate::stats::Refusal;

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
    let Some(pattern) = &args.limit else {
        return Ok(None);
    };
    let resolution = inventory.resolve(pattern);
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
    let names: HashSet<String> = resolution.hosts.into_iter().map(|h| h.name).collect();
    if names.is_empty() {
        anyhow::bail!(
            "Specified inventory, host pattern and/or --limit leaves us with no hosts to target."
        );
    }
    Ok(Some(names))
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
