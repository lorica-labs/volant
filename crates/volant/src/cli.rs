// SPDX-License-Identifier: GPL-3.0-or-later
//! The `playbook` command: the same arguments as `ansible-playbook`, for the subset that exists.

use std::path::PathBuf;

use anstream::ColorChoice;
use clap::Parser;
use tokio::sync::watch;

use crate::executor::{self, DEFAULT_CONNECT_TIMEOUT, RunOptions};
use crate::inventory::Inventory;
use crate::render::Renderer;
use crate::stats::{Stats, exit_code};
use crate::{agent, playbook};

#[derive(Parser, Debug)]
pub struct PlaybookArgs {
    /// Playbook files, run in order.
    #[arg(required = true, value_name = "PLAYBOOK")]
    pub playbooks: Vec<PathBuf>,
    /// Inventory file. Without it only the implicit localhost exists.
    #[arg(short = 'i', long = "inventory", value_name = "PATH")]
    pub inventory: Option<PathBuf>,
    /// Disable coloured output.
    #[arg(long)]
    pub no_color: bool,
    /// Show module results for successful tasks too. Repeatable.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    pub verbose: u8,
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
    let inventory = match &args.inventory {
        Some(path) => Inventory::load(path)?,
        None => Inventory::empty(),
    };
    let playbooks = args
        .playbooks
        .iter()
        .map(|p| playbook::load(p))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let agent = agent::locate()?;
    let mut stats = Stats::default();

    let (stop_tx, stop_rx) = watch::channel(false);
    spawn_signal_watcher(stop_tx);
    let options = RunOptions {
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        stop: stop_rx.clone(),
    };

    'plays: for pb in &playbooks {
        for play in &pb.plays {
            if *stop_rx.borrow() {
                break 'plays;
            }
            let resolution = inventory.resolve(&play.hosts);
            for pattern in &resolution.unmatched {
                out.warning(&format!(
                    "Could not match supplied host pattern, ignoring: {pattern}"
                ));
            }
            executor::run_play(play, resolution.hosts, &agent, &options, out, &mut stats).await?;
        }
    }
    if *stop_rx.borrow() {
        eprintln!("[ERROR]: User interrupted execution");
        out.recap(&stats);
        return Ok(99);
    }
    out.recap(&stats);
    Ok(exit_code(&stats))
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
