// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs one play on its hosts, `linear` unless the play says `free`, `host_pinned` or `debug`:
//! consecutive remote tasks are grouped into batches, `set_fact` and `debug` run on the
//! controller, and output is shown task by task, once every live host has reported that task -
//! under `free` and `host_pinned`, as each line arrives. This file holds what the modules share - the
//! run's settings, the connections kept between plays, and the entry point the command line
//! calls.

mod coordinator;
mod debugger;
mod driver;
mod free;
mod hosts_vars;
mod include;
mod play_checks;
mod play_hosts;
mod prepare;
mod report;
mod run;
mod state;

pub(crate) use run::{RAISED_SKIP, drop_internal_keys};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;

use crate::agent::AgentSource;
use crate::compile::Compiled;
use crate::inventory::Host;
use crate::playbook::Play;
use crate::render::Renderer;
use crate::stats::Stats;
use crate::transport::ConnectionDefaults;

use coordinator::run_batch;
pub(crate) use driver::{expressions, strings, task_strings};
pub(crate) use play_checks::as_bool_value;
use play_checks::{check_gather_keywords, load_play_vars_files, max_fail_percentage, play_flags};
pub(crate) use play_hosts::{Context as HostsContext, is_templated, render_hosts};
pub use state::{Abort, LinkKey, PlayEnd, Reboots, RunState};

/// Ansible's default `timeout`: seconds to establish a connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a host gets to confirm a cancel before it is abandoned.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Settings shared by every play of a run.
#[derive(Clone)]
pub struct RunOptions {
    /// Connection settings every host starts from; its host variables override them.
    pub defaults: ConnectionDefaults,
    /// How many hosts of a play run at once, Ansible's `forks`. Never zero.
    pub forks: usize,
    /// `--force-handlers`, or `[defaults] force_handlers`: whether a host that failed still runs
    /// the handlers it notified. A play saying so itself speaks over this.
    pub force_handlers: bool,
    /// `[volant] batching`, or `VOLANT_BATCHING`: whether a host may carry on through the tasks
    /// between two synchronisation points. Off by default, so the hosts of a batch meet in front
    /// of every task, which is what `linear` means.
    pub batching: bool,
    /// `[volant] host_named_paths`, or `VOLANT_HOST_NAMED_PATHS`: what a task that reads a
    /// controller file does when a managed host named the file.
    pub host_named_paths: crate::config::HostNamedPaths,
    /// `[volant] host_named_dest_dirs`, or `VOLANT_HOST_NAMED_DEST_DIRS`: the controller
    /// directories a `fetch` may write under when a managed host named its `dest`; `None` when
    /// the setting is absent.
    pub host_named_dest_dirs: Option<Vec<PathBuf>>,
    /// Where the sockets of the shared `ssh` connections live, from
    /// [`crate::transport::control_dir`]; `None` when `[volant] ssh_control_master` is off or no
    /// safe directory exists, and every `ssh` then opens its own connection.
    pub control_dir: Option<PathBuf>,
    /// Flips to `true` once when the user interrupts the run, or when a host ends it through
    /// [`Abort::raise`].
    pub stop: watch::Receiver<bool>,
    /// The sending half of `stop`, and the reason a host ended the run, if one did.
    pub abort: Arc<Abort>,
    /// Which hosts a task of this run rebooted, shared by every host's driver.
    pub reboots: Arc<Reboots>,
    /// Where the run's timings go, for `--profile` and `VOLANT_PROFILE_JSON`.
    pub profile: Arc<crate::profile::Profile>,
    /// `--step`: present when each task is asked about, and set once the operator answers `c`,
    /// which ends the questions for the rest of the run.
    pub step: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Runs one play, batch by batch.
///
/// `hosts` is the play's pattern resolved and narrowed by `--limit`, with **nothing filtered
/// out**: the hosts that failed in an earlier play are still in it, because the reference cuts
/// its batches from that list and only then drops the failures from each batch. Cutting the
/// filtered list instead merges two batches into one whenever an earlier play lost a host.
#[expect(
    clippy::too_many_arguments,
    reason = "the play's whole context: eight values with no natural grouping until the controller is split"
)]
#[expect(clippy::too_many_lines)]
pub async fn run_play(
    play: &Play,
    compiled: &Compiled,
    hosts: Vec<Host>,
    agents: &AgentSource,
    options: &RunOptions,
    state: &mut RunState,
    out: &mut Renderer,
    stats: &mut Stats,
) -> anyhow::Result<PlayEnd> {
    // A pattern that matched nothing is the one case with no batch at all: the reference prints
    // the banner and says so, and the play keyword that would have cut the batches is never read.
    if hosts.is_empty() {
        out.play_of(play);
        out.no_hosts();
        return Ok(PlayEnd { stop_run: false });
    }
    let play_vars = {
        let store = state.vars.lock().expect("vars lock");
        store.play_scope(&play.vars)
    };
    let batches =
        crate::compile::batches(&hosts, play.serial.as_ref(), &state.templar, &play_vars)?;
    let max_fail = max_fail_percentage(play, &state.templar, &play_vars)?;
    check_gather_keywords(play, &state.templar, &play_vars)?;
    let flags = play_flags(play, &state.templar, &play_vars)?;
    // `force_handlers` as a template is rendered above and read from the play below, like the
    // literal.
    let resolved;
    let play = match flags.force_handlers {
        Some(on) => {
            resolved = Play {
                force_handlers: Some(on),
                ..play.clone()
            };
            &resolved
        }
        None => play,
    };
    let all: Vec<String> = hosts.iter().map(|h| h.name.clone()).collect();
    // `gathering = smart`: the hosts the cache already answers for leave the `Gathering Facts`
    // step to the others, and a step nobody runs shows nothing at all.
    let masked = {
        let store = state.vars.lock().expect("vars lock");
        let wanting: Vec<String> = all
            .iter()
            .filter(|h| !store.skips_gathering(h))
            .cloned()
            .collect();
        // A `gather_facts` template that rendered to false leaves the step to nobody.
        let gathers = flags.gather_facts != Some(false);
        (compiled.gathers && (wanting.len() < all.len() || !gathers)).then(|| {
            let mut copy = compiled.clone();
            copy.steps[0].hosts = Some(if gathers { wanting } else { Vec::new() }.into());
            copy
        })
    };
    let compiled = masked.as_ref().unwrap_or(compiled);
    let strategy = prepare::Strategy::named(play.strategy.as_deref());
    let mut loose = Vec::new();
    let loosened = strategy.free().then(|| {
        let mut copy = compiled.clone();
        loose = free::loosen(&mut copy);
        copy
    });
    let compiled = loosened.as_ref().unwrap_or(compiled);
    let playbook_dir = state
        .vars
        .lock()
        .expect("vars lock")
        .playbook_dir()
        .to_path_buf();
    for batch in batches {
        // A batch that starts after the operator interrupted the run is a batch that should not
        // start: the drivers of the batch before it have already stopped for the same reason.
        if *options.stop.borrow() {
            break;
        }
        // The reference divides by the batch as the inventory cut it, hosts that failed in an
        // earlier play included, and counts only the failures of this batch.
        let batch_size = batch.len();
        let whole: Vec<String> = batch.iter().map(|h| h.name.clone()).collect();
        let live: Vec<Host> = batch
            .iter()
            .filter(|h| !state.failed_hosts.contains(&h.name))
            .cloned()
            .collect();
        // One banner per batch, measured - and it prints even when the batch has nothing left to
        // run, which is the shape a play gets when every host it resolved has already failed.
        if live.is_empty() {
            out.play_of(play);
            continue;
        }
        // Read here, once per batch about to run and not once for the whole play: see
        // `load_play_vars_files`. `play_hosts` is the play's still-live hosts computed from
        // `state.failed_hosts` as it stands right before this batch starts (this batch and every
        // batch still to come); `batch_hosts` is this batch's own live hosts; `all` stays the
        // play's whole resolved list, batches and failures alike.
        let play_hosts: Vec<String> = hosts
            .iter()
            .filter(|h| !state.failed_hosts.contains(&h.name))
            .map(|h| h.name.clone())
            .collect();
        let batch_hosts: Vec<String> = live.iter().map(|h| h.name.clone()).collect();
        let vars_files = load_play_vars_files(
            play,
            &live,
            &play_hosts,
            &batch_hosts,
            &all,
            &playbook_dir,
            state,
            out,
        )?;
        // After the `vars_files`: measured on ansible-core 2.19.12, an entry that renders to a
        // number or does not render at all stops the run before the banner, with nothing on
        // stdout.
        out.play_of(play);
        // A cache file the run could not read stops it here, after the banner: warning, error,
        // exit 1, no recap.
        let failure = state.vars.lock().expect("vars lock").take_cache_failure();
        if let Some(failure) = failure {
            if let Some(warning) = &failure.warning {
                out.warning(warning, false);
            }
            return Err(crate::stats::Refusal::plain(1, failure.error));
        }
        // The ones of the batch that went unreachable in an earlier play: out of this one, and
        // the only hosts a `clear_host_errors` brings back at its first step.
        let dormant: Vec<String> = whole
            .iter()
            .filter(|h| state.unreachable_hosts.contains(*h))
            .cloned()
            .collect();
        // Measured: under `free` and `host_pinned` no share of failed hosts ends the batch, and
        // the reference says so once, ahead of what the steps themselves warn about.
        if strategy.free() {
            if max_fail.is_some() {
                out.warning(free::MAX_FAIL, false);
            }
            for warning in &loose {
                out.warning(warning, false);
            }
        }
        let limit = max_fail
            .filter(|_| !strategy.free())
            .map(|limit| (limit, batch_size));
        // `host_pinned` with fewer forks than hosts: a fork keeps its host to the end of the
        // play, so the hosts run in waves of `forks`, each a batch of its own under the one
        // banner. A wave waits for its slowest host before the next one starts, where the
        // reference hands a freed fork to the next host at once: the same lines, in another
        // order.
        let waves: Vec<&[Host]> = if strategy.pinned() {
            live.chunks(options.forks.max(1)).collect()
        } else {
            vec![live.as_slice()]
        };
        let mut ended_play = false;
        for (n, wave) in waves.into_iter().enumerate() {
            if n > 0 && (ended_play || options.abort.reason().is_some() || *options.stop.borrow()) {
                break;
            }
            let mut end = run_batch(
                play,
                compiled,
                wave,
                &whole,
                if n == 0 { &dormant[..] } else { &[] },
                &all,
                &vars_files,
                limit,
                None,
                agents,
                options,
                state,
                out,
                stats,
            )
            .await?;
            ended_play |= end.end_play;
            state.unreachable_hosts.extend(end.dark.drain(..));
            // The hosts a `clear_host_errors` brought back run the steps they missed, alone, once the
            // others are done: the reference's linear strategy goes back for them when its list of
            // tasks has run out. Each round can bring back more.
            while !end.revived.is_empty()
                && options.abort.reason().is_none()
                && !*options.stop.borrow()
            {
                let again: Vec<Host> = batch
                    .iter()
                    .filter(|h| end.revived.iter().any(|(name, _)| name == &h.name))
                    .cloned()
                    .collect();
                let starts: HashMap<String, usize> = end.revived.drain(..).collect();
                let steps = end.steps.take().unwrap_or_else(|| compiled.clone());
                let again_names: Vec<String> = again.iter().map(|h| h.name.clone()).collect();
                let files = load_play_vars_files(
                    play,
                    &again,
                    &play_hosts,
                    &again_names,
                    &all,
                    &playbook_dir,
                    state,
                    out,
                )?;
                end = run_batch(
                    play,
                    &steps,
                    &again,
                    &whole,
                    &[],
                    &all,
                    &files,
                    limit,
                    Some(&starts),
                    agents,
                    options,
                    state,
                    out,
                    stats,
                )
                .await?;
                ended_play |= end.end_play;
                state.unreachable_hosts.extend(end.dark.drain(..));
            }
        }
        // What stays unreachable goes back among the failed, forgiven for a while or not.
        for host in &whole {
            if state.unreachable_hosts.contains(host) {
                state.failed_hosts.insert(host.clone());
            }
        }
        if let Some(reason) = options.abort.reason() {
            if options.abort.is_plain() {
                return Err(crate::stats::Refusal::plain(options.abort.code(), reason));
            }
            return Err(crate::stats::Refusal::at(options.abort.code(), reason));
        }
        // The run ends where the reference ends it: a batch that had hosts to run and lost every
        // one of them. A batch that had none to begin with is not that - measured, `serial: 1`
        // over a host that failed in an earlier play prints its banner and the next batch runs.
        if live.iter().all(|h| state.failed_hosts.contains(&h.name)) {
            return Ok(PlayEnd { stop_run: true });
        }
        // `meta: end_play`: the batches after this one never start.
        if ended_play {
            break;
        }
    }
    Ok(PlayEnd { stop_run: false })
}

/// The name Python would give a value, so a message about a mistyped playbook key reads the way
/// the reference's does.
pub(crate) fn python_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

#[cfg(test)]
mod tests;
