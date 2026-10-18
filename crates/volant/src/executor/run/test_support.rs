// SPDX-License-Identifier: GPL-3.0-or-later
//! The fake agents and the item and result builders the tests of `run` share.

use super::channel::{AgentChannel, Relink};
#[cfg(unix)]
use super::links::KeptLink;
use crate::agent::BlobMemory;
use crate::executor::prepare::Item;
use crate::python::{ModulePayload, Union};
use crate::testing::vars;
use crate::vars::HostVars;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use volant_protocol::{BatchOutcome, FromAgent, Task, TaskResult, ToAgent};

pub(super) fn local_item(args: Value) -> Item {
    Item {
        element: None,
        label: None,
        args: vars(args),
        args_untrusted: BTreeSet::new(),
        vars: HostVars::default(),
        environment: BTreeMap::new(),
        ignore_errors: None,
        check_mode: None,
        diff: None,
        skipped: None,
        set_fact_names: std::sync::OnceLock::new(),
        notify: Vec::new(),
    }
}

pub(super) fn result(v: Value) -> TaskResult {
    TaskResult(v.as_object().unwrap().clone())
}

/// A plugin that reconnects, where the test expects none to: every try fails.
pub(super) struct NoRelink;

impl<C> Relink<C> for NoRelink {
    async fn relink(&mut self, _link: &mut C, _: Option<Duration>) -> Result<(), String> {
        Err("this test opens no connection".into())
    }
}

/// An agent that answers what the test scripted, and remembers what a link remembers.
pub(super) struct FakeAgent {
    pub(super) sent: Vec<ToAgent>,
    /// Each blob frame, with the count of messages sent before it.
    pub(super) frames: Vec<(usize, Vec<u8>)>,
    pub(super) answers: std::collections::VecDeque<FromAgent>,
    pub(super) memory: BlobMemory,
    /// Whether a fake that has run out of answers holds the line open instead of closing it,
    /// which is what a real agent busy with a batch does.
    pub(super) hangs_when_empty: bool,
    pub(super) ledger: crate::profile::Ledger,
    /// How long writing one blob frame takes, for a transfer over a slow link.
    pub(super) transfer_takes: Duration,
    /// Set to run batches one at a time, as the agent does.
    pub(super) busy: Option<Box<Busy>>,
    /// Whether the link was given up on.
    pub(super) abandoned: bool,
    /// How many times the closing line of a terminal was asked for.
    pub(super) close_asked: usize,
}

/// Batches run the way `volant-agent`'s runner runs them: one at a time, each `RunBatch` it
/// accepts taking the next script (`None` runs until cancelled), and a `RunBatch` that
/// arrives while another batch runs dropped, as `runner::is_cancelled` drops it.
pub(super) struct Busy {
    pub(super) scripts: std::collections::VecDeque<Option<Vec<FromAgent>>>,
    pub(super) running: Option<u64>,
    pub(super) dropped: Vec<u64>,
    /// Whether a `Cancel` of the running batch is answered with its `BatchDone`.
    pub(super) confirms_cancel: bool,
}

impl FakeAgent {
    /// A fake running `scripts` one batch at a time; see [`Busy`].
    pub(super) fn busy(scripts: Vec<Option<Vec<FromAgent>>>, confirms_cancel: bool) -> Self {
        FakeAgent {
            hangs_when_empty: true,
            busy: Some(Box::new(Busy {
                scripts: scripts.into(),
                running: None,
                dropped: Vec::new(),
                confirms_cancel,
            })),
            ..FakeAgent::answering(Vec::new())
        }
    }
}

/// `msg` as if the agent sent it for batch `id`.
pub(super) fn for_batch(msg: FromAgent, id: u64) -> FromAgent {
    match msg {
        FromAgent::TaskResult {
            index, result, ran, ..
        } => FromAgent::TaskResult {
            batch: id,
            index,
            result,
            ran,
        },
        FromAgent::BatchDone { outcome, .. } => FromAgent::BatchDone { batch: id, outcome },
        other => other,
    }
}

impl FakeAgent {
    pub(super) fn answering(answers: Vec<FromAgent>) -> Self {
        FakeAgent {
            sent: Vec::new(),
            frames: Vec::new(),
            answers: answers.into(),
            memory: BlobMemory::default(),
            hangs_when_empty: false,
            ledger: crate::profile::Ledger::default(),
            transfer_takes: Duration::ZERO,
            busy: None,
            abandoned: false,
            close_asked: 0,
        }
    }

    pub(super) fn puts(&self) -> usize {
        self.sent
            .iter()
            .filter(|m| matches!(m, ToAgent::PutBlob { .. }))
            .count()
    }
}

impl AgentChannel for FakeAgent {
    async fn tty_close(&mut self) -> Option<String> {
        self.close_asked += 1;
        Some("Connection to h closed.".to_string())
    }

    fn memory(&mut self) -> &mut BlobMemory {
        &mut self.memory
    }

    async fn ask(&mut self, msg: &ToAgent) -> std::io::Result<()> {
        self.sent.push(msg.clone());
        if let (Some(busy), ToAgent::RunBatch { id, .. }) = (&mut self.busy, msg) {
            if busy.running.is_some() {
                busy.dropped.push(*id);
            } else {
                busy.running = Some(*id);
                let script = busy.scripts.pop_front().flatten().unwrap_or_default();
                self.answers
                    .extend(script.into_iter().map(|m| for_batch(m, *id)));
            }
        }
        Ok(())
    }

    async fn ask_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if !self.transfer_takes.is_zero() {
            tokio::time::sleep(self.transfer_takes).await;
        }
        self.frames.push((self.sent.len(), bytes.to_vec()));
        Ok(())
    }

    async fn answer(&mut self) -> std::io::Result<Option<FromAgent>> {
        match self.answers.pop_front() {
            Some(answer) => {
                if let (Some(busy), FromAgent::BatchDone { batch, .. }) = (&mut self.busy, &answer)
                    && busy.running == Some(*batch)
                {
                    busy.running = None;
                }
                Ok(Some(answer))
            }
            None if self.hangs_when_empty => std::future::pending().await,
            None => Ok(None),
        }
    }

    async fn stop_batch(&mut self, id: u64, _grace: Duration) -> bool {
        self.sent.push(ToAgent::Cancel { id });
        let Some(busy) = &mut self.busy else {
            return true;
        };
        if busy.running != Some(id) || !busy.confirms_cancel {
            return false;
        }
        busy.running = None;
        self.answers.retain(|m| {
            !matches!(m, FromAgent::TaskResult { batch, .. } | FromAgent::BatchDone { batch, .. } if *batch == id)
        });
        true
    }

    fn timings(&mut self) -> Option<&mut crate::profile::Ledger> {
        Some(&mut self.ledger)
    }

    fn abandon(&mut self) {
        self.abandoned = true;
    }
}

pub(super) fn state(hash: &str, present: bool) -> FromAgent {
    FromAgent::BlobState {
        hash: hash.to_string(),
        present,
    }
}

/// A union holding every module a plugin may run, each keyed by its own short name.
pub(super) fn union_of(hash: &str, modules: &[&str]) -> Union {
    Union {
        modules: modules
            .iter()
            .map(|m| {
                let facts = crate::python::ModuleFacts {
                    module_fqn: format!("ansible.modules.{m}"),
                    ..module_payload(hash).facts
                };
                ((*m).to_string(), facts)
            })
            .collect(),
        ..union_named(hash)
    }
}

/// What the agent answers for a batch of one task that ran to its end.
pub(super) fn one_result(batch: u64, result: Value) -> [FromAgent; 2] {
    [
        FromAgent::TaskResult {
            batch,
            index: 0,
            result: TaskResult(vars(result)),
            ran: None,
        },
        FromAgent::BatchDone {
            batch,
            outcome: BatchOutcome::Completed,
        },
    ]
}

/// Every `RunBatch` the fake was sent, as the tasks each one carried.
pub(super) fn batches_sent(agent: &FakeAgent) -> Vec<&[Task]> {
    agent
        .sent
        .iter()
        .filter_map(|m| match m {
            ToAgent::RunBatch { tasks, .. } => Some(tasks.as_slice()),
            _ => None,
        })
        .collect()
}

pub(super) fn union_named(hash: &str) -> Union {
    Union {
        hash: hash.to_string(),
        zip_b64: "UEsDBA==".to_string(),
        modules: BTreeMap::new(),
        refused: BTreeMap::new(),
        natives: crate::python::Natives {
            enabled: true,
            facts: crate::python::Facts::Auto,
        },
    }
}

/// A scripted agent that raises the run's stop as it hands over the `BatchDone` of batch
/// `after`, which is the moment between two attempts of a retried plugin item.
pub(super) struct StopsAfter {
    pub(super) agent: FakeAgent,
    pub(super) stop: watch::Sender<bool>,
    pub(super) after: u64,
}

impl AgentChannel for StopsAfter {
    fn memory(&mut self) -> &mut BlobMemory {
        self.agent.memory()
    }

    async fn ask(&mut self, msg: &ToAgent) -> std::io::Result<()> {
        self.agent.ask(msg).await
    }

    async fn ask_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.agent.ask_bytes(bytes).await
    }

    async fn answer(&mut self) -> std::io::Result<Option<FromAgent>> {
        let answer = self.agent.answer().await?;
        if let Some(FromAgent::BatchDone { batch, .. }) = &answer
            && *batch == self.after
        {
            let _ = self.stop.send(true);
        }
        Ok(answer)
    }

    async fn stop_batch(&mut self, id: u64, grace: Duration) -> bool {
        self.agent.stop_batch(id, grace).await
    }
}

pub(super) fn modules_sent(agent: &FakeAgent) -> Vec<&str> {
    batches_sent(agent)
        .iter()
        .map(|b| b[0].module.as_str())
        .collect()
}

pub(super) fn bare_item() -> Item {
    Item {
        element: None,
        label: None,
        args: Map::new(),
        args_untrusted: BTreeSet::new(),
        vars: HostVars::default(),
        environment: BTreeMap::new(),
        ignore_errors: None,
        check_mode: None,
        diff: None,
        skipped: None,
        set_fact_names: std::sync::OnceLock::new(),
        notify: Vec::new(),
    }
}

pub(super) fn module_payload(blob: &str) -> ModulePayload {
    ModulePayload {
        blob: blob.to_string(),
        facts: crate::python::ModuleFacts {
            module_fqn: "ansible.modules.ping".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            core: true,
        },
        force_python: false,
    }
}

/// What one connection attempt of a scripted reconnection does.
pub(super) enum Try {
    /// A fresh link comes up, answering what this agent was scripted to.
    Up(FakeAgent),
    /// No link, for this reason.
    Down(&'static str),
    /// No answer at all, for as long as anybody waits.
    Hangs,
}

/// Reconnections a test scripted, one entry per try, then `after` for every try past them.
/// Keeps every link it replaced, so a test reads what each one was sent.
pub(super) struct Scripted {
    pub(super) tries: std::collections::VecDeque<Try>,
    pub(super) after: Option<fn() -> FakeAgent>,
    pub(super) attempts: usize,
    /// The `connect_timeout` each try was given.
    pub(super) connect_timeouts: Vec<Option<Duration>>,
    pub(super) retired: Vec<FakeAgent>,
    /// Raised a tenth of a second after try number `stop_on` starts, for a test of the run
    /// being interrupted while it waits.
    pub(super) stop: Option<watch::Sender<bool>>,
    pub(super) stop_on: usize,
    /// A millisecond in place of the reference's seconds, so the schedule's shape is tested
    /// without its length (`the_pause_doubles_from_one_second_up_to_twelve`), unless a test
    /// needs a pause long enough to be interrupted.
    pub(super) pause: Duration,
}

impl Scripted {
    pub(super) fn new(tries: Vec<Try>) -> Self {
        Scripted {
            tries: tries.into(),
            after: None,
            attempts: 0,
            connect_timeouts: Vec::new(),
            retired: Vec::new(),
            stop: None,
            stop_on: 1,
            pause: Duration::from_millis(1),
        }
    }
}

impl Relink<FakeAgent> for Scripted {
    async fn relink(
        &mut self,
        link: &mut FakeAgent,
        connect_timeout: Option<Duration>,
    ) -> Result<(), String> {
        self.attempts += 1;
        self.connect_timeouts.push(connect_timeout);
        if self.attempts == self.stop_on
            && let Some(stop) = self.stop.take()
        {
            // A moment later, so that it lands inside whatever wait follows this try.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = stop.send(true);
            });
        }
        let next = self
            .tries
            .pop_front()
            .or_else(|| self.after.map(|make| Try::Up(make())));
        match next {
            Some(Try::Up(fresh)) => {
                self.retired.push(std::mem::replace(link, fresh));
                Ok(())
            }
            Some(Try::Down(why)) => Err(why.to_string()),
            Some(Try::Hangs) => std::future::pending().await,
            None => Err("the script ran out".to_string()),
        }
    }

    fn pause(&self, _failed: u32) -> Duration {
        self.pause
    }
}

/// Every `raw` line and module a set of links was sent, in order.
pub(super) fn sent_lines(links: &[&FakeAgent]) -> Vec<Vec<String>> {
    links
        .iter()
        .map(|agent| {
            batches_sent(agent)
                .iter()
                .map(|b| match b[0].module.as_str() {
                    "raw" => b[0].args["_raw_params"].as_str().unwrap_or("").to_string(),
                    module => module.to_string(),
                })
                .collect()
        })
        .collect()
}

pub(super) fn raw_answer(batch: u64, rc: i64, stdout: &str) -> [FromAgent; 2] {
    one_result(
        batch,
        json!({"rc": rc, "stdout": stdout, "stderr": "", "changed": true}),
    )
}

/// A kept link in a test: how it answers its check, and a record of whether it was asked.
#[cfg(unix)]
pub(super) struct FakeKept {
    pub(super) answer: FakeAnswer,
    pub(super) asked: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(unix)]
pub(super) enum FakeAnswer {
    Alive,
    /// Its master is gone: end of file at once.
    Gone,
    /// Its master rides a connection the host dropped: no answer, ever.
    Wedged,
}

#[cfg(unix)]
impl FakeKept {
    pub(super) fn new(answer: FakeAnswer) -> (Self, Arc<std::sync::atomic::AtomicBool>) {
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let link = FakeKept {
            answer,
            asked: Arc::clone(&asked),
        };
        (link, asked)
    }
}

#[cfg(unix)]
impl KeptLink for FakeKept {
    async fn handshake(&mut self) -> anyhow::Result<()> {
        self.asked.store(true, std::sync::atomic::Ordering::SeqCst);
        match self.answer {
            FakeAnswer::Alive => Ok(()),
            FakeAnswer::Gone => anyhow::bail!("the agent closed the connection"),
            FakeAnswer::Wedged => std::future::pending().await,
        }
    }

    async fn shutdown(self) {}
}
