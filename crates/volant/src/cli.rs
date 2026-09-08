// SPDX-License-Identifier: GPL-3.0-or-later
//! The `playbook` command: the same arguments as `ansible-playbook`, for the subset that exists.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anstream::ColorChoice;
use clap::Parser;
use tokio::sync::watch;

use crate::config::Config;
use crate::executor::{self, RunOptions, RunState};
use crate::inventory::{Host, Inventory};
use crate::render::Renderer;
use crate::stats::{Stats, exit_code};
use crate::template::Templar;
use crate::transport::{ConnectionDefaults, Transport};
use crate::vars::VarStore;
use crate::{agent, playbook};

#[derive(Parser, Debug)]
pub struct PlaybookArgs {
    /// Playbook files, run in order.
    #[arg(required = true, value_name = "PLAYBOOK")]
    pub playbooks: Vec<PathBuf>,
    /// Inventory file. Without it only the implicit localhost exists.
    #[arg(short = 'i', long = "inventory", value_name = "PATH")]
    pub inventory: Option<PathBuf>,
    /// Extra variables: `key=value` pairs, inline JSON or YAML, or `@file`. Repeatable.
    #[arg(short = 'e', long = "extra-vars", value_name = "VARS")]
    pub extra_vars: Vec<String>,
    /// Limit the play to this host pattern.
    #[arg(short = 'l', long = "limit", value_name = "SUBSET")]
    pub limit: Option<String>,
    /// Disable coloured output.
    #[arg(long)]
    pub no_color: bool,
    /// Show module results for successful tasks too. Repeatable.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Log in to remote hosts as this user.
    #[arg(short = 'u', long = "user", value_name = "REMOTE_USER")]
    pub user: Option<String>,
    /// Private key file for ssh authentication.
    #[arg(long = "private-key", value_name = "PRIVATE_KEY_FILE")]
    pub private_key: Option<PathBuf>,
    /// Seconds to wait for a connection.
    #[arg(short = 'T', long = "timeout", value_name = "TIMEOUT")]
    pub timeout: Option<u64>,
    /// Number of hosts to run at once.
    #[arg(short = 'f', long = "forks", value_name = "FORKS")]
    pub forks: Option<usize>,
}

/// Runs the playbooks and returns the process exit code.
pub fn run(args: PlaybookArgs) -> i32 {
    let choice = if args.no_color {
        ColorChoice::Never
    } else {
        ColorChoice::Auto
    };
    let mut out = Renderer::new(choice, args.verbose);
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("ERROR! {err}");
            return 250;
        }
    };
    match runtime.block_on(run_all(&args, &mut out)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("ERROR! {err:#}");
            1
        }
    }
}

async fn run_all(args: &PlaybookArgs, out: &mut Renderer) -> anyhow::Result<i32> {
    let config = Config::load();
    // Refused before anything is loaded, the way the reference refuses it, whether it comes
    // from the command line, the environment or `ansible.cfg`. Exit 2: `Cli::parse()` already
    // exits 2 for `-f abc` or `-f -1` (clap's own default for a bad argument), so a refused `0`
    // through this check stays on the same code rather than inventing a second one for the same
    // flag; a refusal never prints a `PLAY` header or a recap, so it cannot be mistaken for a
    // failed task, which is exit 2's other meaning.
    let forks = args.forks.unwrap_or(config.forks);
    if forks == 0 {
        eprintln!("ERROR! The number of processes (--forks) must be >= 1");
        return Ok(2);
    }
    let inventory_path = args.inventory.clone().or(config.inventory);
    let inventory = match &inventory_path {
        Some(path) => Inventory::load(path)?,
        None => Inventory::empty(),
    };
    let playbooks = args
        .playbooks
        .iter()
        .map(|p| playbook::load(p))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let agents = agent::AgentSource::discover();
    let defaults = ConnectionDefaults {
        remote_user: args.user.clone().or(config.remote_user),
        private_key: args.private_key.clone().or(config.private_key_file),
        host_key_checking: config.host_key_checking,
        remote_tmp: config.remote_tmp,
        connect_timeout: args
            .timeout
            .map(std::time::Duration::from_secs)
            .unwrap_or(config.timeout),
    };
    let mut stats = Stats::default();

    let (stop_tx, stop_rx) = watch::channel(false);
    spawn_signal_watcher(stop_tx);
    let options = RunOptions {
        defaults,
        forks,
        stop: stop_rx.clone(),
    };

    let playbook_dir = base_dir(&args.playbooks[0]);
    let cwd = std::env::current_dir()?;
    let extra = crate::vars::parse_extra_vars(&args.extra_vars, &cwd)?;
    let mut store = VarStore::new(&inventory, inventory_path.as_deref(), &playbook_dir, extra)?;
    store.set_forks(forks);
    let mut state = RunState {
        templar: Arc::new(Templar::new(playbook_dir.clone())),
        vars: Arc::new(Mutex::new(store)),
        failed_hosts: HashSet::new(),
        verbosity: args.verbose,
        links: HashMap::new(),
    };

    let limit: Option<HashSet<String>> = match &args.limit {
        None => None,
        Some(pattern) => {
            let names: HashSet<String> = inventory
                .resolve(pattern)
                .hosts
                .into_iter()
                .map(|h| h.name)
                .collect();
            if names.is_empty() {
                anyhow::bail!(
                    "Specified inventory, host pattern and/or --limit leaves us with no hosts to target."
                );
            }
            Some(names)
        }
    };

    // `resolve` recomputes inventory-load-time warnings (e.g. a host/group homonym) on every
    // call, matching Ansible's own semantics for a single resolution; but a playbook with N plays
    // calls `resolve` N times, and Ansible only ever prints such a warning once per run, at
    // inventory load. Deduplicating here, at the point of printing, keeps `resolve`'s contract
    // (its `Resolution.warnings` always reflects the truth for that one call, which callers other
    // than this CLI loop may rely on) while matching the reference's once-per-run output.
    let mut warned: HashSet<String> = HashSet::new();
    // A controller with no local agent can still drive remote hosts, so the local agent only
    // has to be there once a play actually targets a host that runs one here. Saying so before
    // that play starts beats one `UNREACHABLE` per local host.
    let mut local_agent_checked = false;
    let mut current_dir = playbook_dir;
    'plays: for (path, pb) in args.playbooks.iter().zip(&playbooks) {
        let dir = base_dir(path);
        if dir != current_dir {
            state.templar = Arc::new(Templar::new(dir.clone()));
            state.vars.lock().expect("vars lock").rebase(&dir)?;
            current_dir = dir;
        }
        for play in &pb.plays {
            if *stop_rx.borrow() {
                break 'plays;
            }
            let resolution = inventory.resolve(&play.hosts);
            for warning in &resolution.warnings {
                if warned.insert(warning.clone()) {
                    out.warning(warning);
                }
            }
            for pattern in &resolution.unmatched {
                out.warning(&format!(
                    "Could not match supplied host pattern, ignoring: {pattern}"
                ));
            }
            let hosts: Vec<Host> = match &limit {
                Some(allowed) => resolution
                    .hosts
                    .into_iter()
                    .filter(|h| allowed.contains(&h.name))
                    .collect(),
                None => resolution.hosts,
            };
            if !local_agent_checked
                && hosts.iter().any(|h| {
                    matches!(
                        Transport::for_host(h, &options.defaults),
                        Ok(Transport::Local)
                    )
                })
            {
                agents.local()?;
                local_agent_checked = true;
            }
            if let Err(err) =
                executor::run_play(play, hosts, &agents, &options, &mut state, out, &mut stats)
                    .await
            {
                // Closing them here rather than letting the state drop keeps the wait for
                // each agent off a blocking drop inside the runtime.
                state.shutdown_links().await;
                return Err(err);
            }
        }
    }
    state.shutdown_links().await;
    if *stop_rx.borrow() {
        eprintln!("[ERROR]: User interrupted execution");
        out.recap(&stats);
        return Ok(99);
    }
    out.recap(&stats);
    Ok(exit_code(&stats))
}

/// A playbook's own directory, absolute where the filesystem allows it: `group_vars/`,
/// `host_vars/`, relative `vars_files` entries, lookups and the `playbook_dir` variable all
/// resolve against it, so it follows the playbook being run rather than the first one.
fn base_dir(playbook: &Path) -> PathBuf {
    let dir = match playbook.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    dir.canonicalize().unwrap_or(dir)
}

/// Ctrl-C and SIGTERM both request a clean stop: running batches are cancelled, the recap is
/// printed, and the process exits with ansible-playbook's code 99.
fn spawn_signal_watcher(stop: watch::Sender<bool>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(term) => term,
                Err(_) => return,
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = stop.send(true);
    });
}
