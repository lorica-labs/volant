// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs one play on its hosts with the `linear` strategy: every host runs the same batch,
//! output is shown task by task once every live host has reported that task.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use tokio::sync::mpsc;
use volant_protocol::{BatchOutcome, FromAgent, Task, TaskResult, ToAgent};

use crate::agent::AgentLink;
use crate::inventory::Host;
use crate::playbook::Play;
use crate::render::Renderer;
use crate::stats::{Outcome, Stats};
use crate::transport::Transport;

enum Event {
    Result {
        host: String,
        index: usize,
        result: TaskResult,
    },
    Finished {
        host: String,
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
        workers.push(tokio::spawn(async move {
            drive_host(name, transport, agent, tasks, tx).await
        }));
    }
    drop(tx);

    let order: Vec<String> = hosts.iter().map(|h| h.name.clone()).collect();
    let mut pending: HashMap<String, BTreeMap<usize, TaskResult>> = HashMap::new();
    let mut finished: HashMap<String, Option<usize>> = HashMap::new();

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
                if finished.contains_key(host) {
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
                    Some(Event::Finished { host, stopped_at }) => {
                        finished.insert(host, stopped_at);
                    }
                    Some(Event::Unreachable { host, msg }) => {
                        if !header_shown {
                            out.task(&task.name);
                            header_shown = true;
                        }
                        stats.unreachable(&host);
                        out.unreachable(&host, &msg);
                        finished.insert(host, Some(index));
                    }
                    None => break,
                }
            }
        }
        if finished.len() == order.len() && pending.values().all(BTreeMap::is_empty) {
            break;
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
    tx: mpsc::Sender<Event>,
) {
    let mut link: AgentLink = match connect(&transport, &agent).await {
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
    let stopped_at = loop {
        match link.recv().await {
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
                break match outcome {
                    BatchOutcome::Completed => None,
                    BatchOutcome::Failed { at } | BatchOutcome::Cancelled { at } => Some(at),
                };
            }
            Ok(Some(FromAgent::Log { message, .. })) => eprintln!("[{host}] {message}"),
            Ok(Some(FromAgent::Ready { .. })) => {}
            Ok(None) | Err(_) => break Some(0),
        }
    };
    link.shutdown().await;
    let _ = tx.send(Event::Finished { host, stopped_at }).await;
}

async fn connect(transport: &Transport, agent: &Path) -> anyhow::Result<AgentLink> {
    let mut link = transport.connect(agent).await?;
    link.handshake().await?;
    Ok(link)
}
