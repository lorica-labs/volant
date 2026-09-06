// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs one play on its hosts with the `linear` strategy: every host runs the same batch,
//! output is shown task by task once every live host has reported that task.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use volant_protocol::{BatchOutcome, FromAgent, Task, TaskResult, ToAgent};

use crate::agent::AgentLink;
use crate::inventory::Host;
use crate::playbook::Play;
use crate::render::Renderer;
use crate::stats::{Outcome, Stats};
use crate::transport::Transport;

/// Ansible's default `timeout`: seconds to establish a connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a host gets to confirm a cancel before it is abandoned.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Settings shared by every play of a run.
#[derive(Clone)]
pub struct RunOptions {
    pub connect_timeout: Duration,
    /// Flips to `true` once when the user interrupts the run.
    pub stop: watch::Receiver<bool>,
}

enum Event {
    Result {
        host: String,
        index: usize,
        result: TaskResult,
    },
    /// The batch ended. `stopped_at` is the index of the task that failed or was cancelled,
    /// `None` when every task ran. Not read by the coordinator yet; a later task uses it.
    Finished {
        host: String,
        #[allow(dead_code)]
        stopped_at: Option<usize>,
    },
    Unreachable {
        host: String,
        msg: String,
    },
}

pub async fn run_play(
    play: &Play,
    hosts: Vec<Host>,
    agent: &Path,
    options: &RunOptions,
    out: &mut Renderer,
    stats: &mut Stats,
) -> anyhow::Result<()> {
    out.play(&play.name);
    if hosts.is_empty() {
        out.no_hosts();
        return Ok(());
    }
    if play.gather_facts {
        out.warning("gather_facts is not available in this release; continuing without facts");
    }
    // Refuse unknown connections before touching any host.
    let transports: Vec<Transport> = hosts
        .iter()
        .map(Transport::for_host)
        .collect::<anyhow::Result<_>>()?;

    let tasks: Vec<Task> = play
        .tasks
        .iter()
        .map(|t| Task {
            module: t.module.clone(),
            args: t.args.clone(),
            ignore_errors: t.ignore_errors,
            timeout: None,
        })
        .collect();
    let ignore: Vec<bool> = play.tasks.iter().map(|t| t.ignore_errors).collect();

    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut workers = Vec::new();
    for (host, transport) in hosts.iter().zip(transports) {
        let tx = tx.clone();
        let name = host.name.clone();
        let agent = agent.to_path_buf();
        let tasks = tasks.clone();
        let options = options.clone();
        workers.push(tokio::spawn(async move {
            drive_host(name, transport, agent, tasks, options, tx).await
        }));
    }
    drop(tx);

    let order: Vec<String> = hosts.iter().map(|h| h.name.clone()).collect();
    let mut pending: HashMap<String, BTreeMap<usize, TaskResult>> = HashMap::new();
    let mut finished: HashSet<String> = HashSet::new();

    for (index, task) in play.tasks.iter().enumerate() {
        let mut header_shown = false;
        for host in &order {
            loop {
                if let Some(result) = pending.get_mut(host).and_then(|m| m.remove(&index)) {
                    if !header_shown {
                        out.task(&task.name);
                        header_shown = true;
                    }
                    let outcome = classify(&result, ignore[index]);
                    stats.record(host, outcome);
                    out.result(host, outcome, &result);
                    break;
                }
                if finished.contains(host) {
                    break;
                }
                match rx.recv().await {
                    Some(Event::Result {
                        host,
                        index,
                        result,
                    }) => {
                        pending.entry(host).or_default().insert(index, result);
                    }
                    Some(Event::Finished { host, .. }) => {
                        finished.insert(host);
                    }
                    Some(Event::Unreachable { host, msg }) => {
                        if !header_shown {
                            out.task(&task.name);
                            header_shown = true;
                        }
                        stats.unreachable(&host);
                        out.unreachable(&host, &msg);
                        finished.insert(host);
                    }
                    None => break,
                }
            }
        }
        if finished.len() == order.len() && pending.values().all(BTreeMap::is_empty) {
            break;
        }
    }
    // The channel is bounded, so a host still owing a send would block forever if reading
    // stopped here. Drain until every worker has dropped its sender, which is also the last
    // chance to report a host that died after its final result.
    while let Some(event) = rx.recv().await {
        if let Event::Unreachable { host, msg } = event {
            stats.unreachable(&host);
            out.unreachable(&host, &msg);
        }
    }
    for worker in workers {
        let _ = worker.await;
    }
    Ok(())
}

fn classify(result: &TaskResult, ignore_errors: bool) -> Outcome {
    if result.failed() {
        if ignore_errors {
            Outcome::Ignored
        } else {
            Outcome::Failed
        }
    } else if result.skipped() {
        Outcome::Skipped
    } else if result.changed() {
        Outcome::Changed
    } else {
        Outcome::Ok
    }
}

async fn drive_host(
    host: String,
    transport: Transport,
    agent: std::path::PathBuf,
    tasks: Vec<Task>,
    options: RunOptions,
    tx: mpsc::Sender<Event>,
) {
    let mut stop = options.stop.clone();
    let mut link: AgentLink = match connect(&transport, &agent, options.connect_timeout).await {
        Ok(link) => link,
        Err(err) => {
            let _ = tx
                .send(Event::Unreachable {
                    host,
                    msg: format!("{err:#}"),
                })
                .await;
            return;
        }
    };
    if let Err(err) = link.send(&ToAgent::RunBatch { id: 1, tasks }).await {
        let _ = tx
            .send(Event::Unreachable {
                host,
                msg: format!("sending batch: {err}"),
            })
            .await;
        return;
    }
    // An end that is not a `BatchDone` means the agent died, the pipe closed or a frame
    // failed to decode: the host is unreachable from here on, not quietly done.
    let ended = loop {
        let received = tokio::select! {
            received = link.recv() => received,
            _ = stop.changed() => {
                // The user asked to stop: give the agent a chance to end cleanly.
                let confirmed = link.cancel(1, CANCEL_GRACE).await;
                break Ok(if confirmed { Some(0) } else { None });
            }
        };
        match received {
            Ok(Some(FromAgent::TaskResult { index, result, .. })) => {
                let _ = tx
                    .send(Event::Result {
                        host: host.clone(),
                        index,
                        result,
                    })
                    .await;
            }
            Ok(Some(FromAgent::BatchDone { outcome, .. })) => {
                break Ok(match outcome {
                    BatchOutcome::Completed => None,
                    BatchOutcome::Failed { at } | BatchOutcome::Cancelled { at } => Some(at),
                });
            }
            Ok(Some(FromAgent::Log { message, .. })) => eprintln!("[{host}] {message}"),
            Ok(Some(FromAgent::Ready { .. })) => {}
            Ok(None) => break Err("agent stopped before the batch finished".to_string()),
            Err(err) => break Err(format!("reading from the agent: {err}")),
        }
    };
    link.shutdown().await;
    let _ = tx
        .send(match ended {
            Ok(stopped_at) => Event::Finished { host, stopped_at },
            Err(msg) => Event::Unreachable { host, msg },
        })
        .await;
}

async fn connect(
    transport: &Transport,
    agent: &Path,
    timeout: Duration,
) -> anyhow::Result<AgentLink> {
    let mut link = transport.connect(agent).await?;
    tokio::time::timeout(timeout, link.handshake())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "no answer from the agent after {} seconds",
                timeout.as_secs()
            )
        })??;
    Ok(link)
}
