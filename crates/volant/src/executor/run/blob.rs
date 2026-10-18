// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeSet;
use std::time::Duration;

use volant_protocol::{BatchOutcome, FromAgent, Task, TaskResult, ToAgent};

use crate::agent::{BlobFrame, BlobMemory};
use crate::python::Union;

use super::channel::AgentChannel;

/// Makes sure the agent behind this link holds the payload named `hash`, sending it only when
/// it does not.
///
/// Once per link, never once per batch: the agent answers `has_blob` by hashing the file it
/// has, so a yes means the bytes are right, and the 631 KB of a union blob has no business
/// going up again for every batch of a play. A refusal is remembered as a refusal, so the
/// batches behind it fail on what the agent said rather than sending the same rejected payload
/// again.
pub(super) async fn ensure_blob<C: AgentChannel>(
    link: &mut C,
    host: &str,
    hash: &str,
    zip_b64: &str,
    logs: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(seen) = link.memory().seen(hash) {
        return seen;
    }
    // Deflated once per run, when the union cache stored or loaded the entry.
    let frame = crate::agent::frame(hash, zip_b64);
    let state = match place_blob(link, host, hash, frame, false, logs).await {
        Ok(true) => Ok(()),
        // The agent logs why on its way to saying no, and that line is already in `logs`.
        Ok(false) => Err(format!(
            "the agent refused the module payload {hash}; it holds no payload to run this task from"
        )),
        Err(err) => Err(err),
    };
    link.memory().remember(hash, state.clone());
    state
}

/// Asks whether the agent holds `hash`, and sends it when it does not: whether it holds it now.
/// A `staged` file is sent without asking. `frame` is only awaited when the blob goes up.
///
/// Remembers nothing. That is [`ensure_blob`]'s to do for a payload, and is never done for a
/// file a sub-task stages: the agent takes that one out of its cache to hand it to the module,
/// so a link that remembered it would send the next task after a file that is no longer there.
/// Nor is a file asked for: two links to one host account share the agent's cache, and a yes
/// about a file the other link is about to consume is the same stale answer.
async fn place_blob<C: AgentChannel>(
    link: &mut C,
    host: &str,
    hash: &str,
    frame: impl Future<Output = Result<std::sync::Arc<BlobFrame>, String>>,
    staged: bool,
    logs: &mut Vec<String>,
) -> Result<bool, String> {
    let has = ToAgent::HasBlob { hash: hash.into() };
    if !staged && blob_state(link, host, hash, &has, None, logs).await? {
        return Ok(true);
    }
    let frame = frame.await?;
    let put = ToAgent::PutBlob {
        hash: hash.into(),
        len: frame.len,
        encoding: frame.encoding,
        staged,
    };
    blob_state(link, host, hash, &put, Some(&frame.bytes), logs).await
}

/// Puts on the host every file a sub-task stages, whatever the link put there before: the
/// hashes the agent now holds, or why one is not there, in the agent's own words.
pub(super) async fn stage_files<C: AgentChannel>(
    link: &mut C,
    host: &str,
    files: &[(String, crate::action_plugins::FileBlob)],
    logs: &mut Vec<String>,
) -> Result<BTreeSet<String>, String> {
    let mut placed = BTreeSet::new();
    for (arg, blob) in files {
        let before = logs.len();
        // Framed once for every host that is sent the same bytes.
        let frame = crate::agent::frame(&blob.hash, &blob.b64);
        let placed_now = place_blob(link, host, &blob.hash, frame, true, logs)
            .await
            .map_err(|err| format!("staging the file for '{arg}': {err}"))?;
        if !placed_now {
            let prefix = format!("[{host}] ");
            let why: Vec<&str> = logs[before..]
                .iter()
                .map(|line| line.strip_prefix(&prefix).unwrap_or(line))
                .collect();
            let why = if why.is_empty() {
                "it gave no reason".to_string()
            } else {
                why.join("; ")
            };
            return Err(format!(
                "the agent could not store the file for '{arg}' ({}): {why}",
                blob.hash
            ));
        }
        placed.insert(blob.hash.clone());
    }
    Ok(placed)
}

/// How long the agent has to move a staged file into place. The move is a local copy and a
/// rename; a host that takes longer than this is hung, and the task fails rather than waits.
pub(super) const PUT_FILE_WAIT: Duration = Duration::from_secs(120);

/// Puts `bytes` at the absolute path `remote` of the host, with `mode` (0600 when `None`):
/// staged first, then moved into place by the agent through a temporary file and a rename.
/// Whether the content differs from what the path held.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the plugins put files with the item's time as the patience"
    )
)]
pub(super) async fn put_file<C: AgentChannel>(
    link: &mut C,
    host: &str,
    bytes: &[u8],
    remote: &str,
    mode: Option<u32>,
    logs: &mut Vec<String>,
) -> Result<bool, String> {
    put_file_within(link, host, bytes, remote, mode, logs, PUT_FILE_WAIT).await
}

/// [`put_file`] with the agent's time to move the file given.
pub(super) async fn put_file_within<C: AgentChannel>(
    link: &mut C,
    host: &str,
    bytes: &[u8],
    remote: &str,
    mode: Option<u32>,
    logs: &mut Vec<String>,
    patience: Duration,
) -> Result<bool, String> {
    let hash = blake3::hash(bytes).to_hex().to_string();
    let before = logs.len();
    let frame = async { Ok(std::sync::Arc::new(BlobFrame::packed(bytes.to_vec()))) };
    let placed = place_blob(link, host, &hash, frame, true, logs)
        .await
        .map_err(|err| format!("staging the file for '{remote}': {err}"))?;
    if !placed {
        let prefix = format!("[{host}] ");
        let why: Vec<&str> = logs[before..]
            .iter()
            .map(|line| line.strip_prefix(&prefix).unwrap_or(line))
            .collect();
        let why = if why.is_empty() {
            "it gave no reason".to_string()
        } else {
            why.join("; ")
        };
        return Err(format!(
            "the agent could not store the file for '{remote}' ({hash}): {why}"
        ));
    }
    let put = ToAgent::PutFile {
        hash: hash.clone(),
        path: remote.into(),
        mode,
    };
    link.ask(&put)
        .await
        .map_err(|err| format!("asking the agent to put the file at '{remote}': {err}"))?;
    let wait = async {
        loop {
            match link.answer().await {
                Ok(Some(FromAgent::PutFileDone {
                    hash: named,
                    changed,
                    error,
                })) if named == hash => {
                    return match error {
                        None => Ok(changed),
                        Some(why) => Err(why),
                    };
                }
                Ok(Some(FromAgent::Log { message, .. })) => {
                    logs.push(format!("[{host}] {message}"));
                }
                Ok(Some(FromAgent::Ready { .. })) => {}
                Ok(Some(other)) => {
                    return Err(format!(
                        "the agent answered {other:?} while asked to put the file at '{remote}'"
                    ));
                }
                Ok(None) => {
                    return Err(format!(
                        "the agent stopped while putting the file at '{remote}'"
                    ));
                }
                Err(err) => {
                    return Err(format!(
                        "reading the agent's answer about the file at '{remote}': {err}"
                    ));
                }
            }
        }
    };
    if let Ok(done) = tokio::time::timeout(patience, wait).await {
        done
    } else {
        link.abandon();
        Err(format!(
            "the agent did not put the file at '{remote}' within {} seconds",
            patience.as_secs()
        ))
    }
}

/// Asks one question about a blob and reads the one answer to it.
///
/// Anything else the agent says on the way is either a line to show or a message that does not
/// belong to this exchange; an answer about another blob is the streams having desynchronised,
/// which is a hard error rather than something to read as a yes. The words say "blob" because
/// the blob may be a module payload or a file a sub-task stages; each caller says which.
async fn blob_state<C: AgentChannel>(
    link: &mut C,
    host: &str,
    hash: &str,
    msg: &ToAgent,
    frame: Option<&[u8]>,
    logs: &mut Vec<String>,
) -> Result<bool, String> {
    link.ask(msg)
        .await
        .map_err(|err| format!("asking the agent about the blob {hash}: {err}"))?;
    if let Some(frame) = frame {
        link.ask_bytes(frame)
            .await
            .map_err(|err| format!("sending the blob {hash}: {err}"))?;
    }
    loop {
        match link.answer().await {
            Ok(Some(FromAgent::BlobState {
                hash: named,
                present,
            })) if named == hash => {
                return Ok(present);
            }
            Ok(Some(FromAgent::BlobState { hash: named, .. })) => {
                return Err(format!(
                    "the agent answered about the blob {named} while asked about {hash}"
                ));
            }
            Ok(Some(FromAgent::Log { message, .. })) => logs.push(format!("[{host}] {message}")),
            // A frame belonging to a batch, read while a blob question is outstanding, means the
            // two streams are out of step: skipping it would leave the next batch's answers
            // being read as this question's, and a stale `BlobState` behind it would pass for a
            // yes. Latent until something runs two batches over a link whose first did not
            // finish - which is what this pull request adds paths for.
            Ok(Some(FromAgent::TaskResult { batch, index, .. })) => {
                return Err(format!(
                    "the agent sent the result of task {index} of batch {batch} while asked about the blob {hash}"
                ));
            }
            Ok(Some(FromAgent::BatchDone { batch, .. })) => {
                return Err(format!(
                    "the agent ended batch {batch} while asked about the blob {hash}"
                ));
            }
            Ok(Some(FromAgent::Ready { .. })) => {}
            Ok(Some(FromAgent::PutFileDone { .. })) => {
                return Err(format!(
                    "the agent ended a put_file while asked about the blob {hash}"
                ));
            }
            Ok(None) => {
                return Err(format!(
                    "the agent stopped while being asked about the blob {hash}"
                ));
            }
            Err(err) => {
                return Err(format!(
                    "reading the agent's answer about the blob {hash}: {err}"
                ));
            }
        }
    }
}

/// Every payload in a batch names a blob this link confirmed.
///
/// A task naming anything else is a controller that built a payload and never made sure the
/// host had it - a bug here, not a fault of the host - so it fails naming the hash instead of
/// being sent for the agent to fail on, where it would read as the module's own failure.
pub(super) fn payloads_confirmed(tasks: &[Task], memory: &BlobMemory) -> Result<(), String> {
    for task in tasks {
        if let Some(payload) = &task.payload
            && !memory.holds(&payload.blob)
        {
            return Err(format!(
                "task '{}' carries the module payload {} which this link never confirmed",
                task.module, payload.blob
            ));
        }
    }
    Ok(())
}

/// Every file a task stages was put on the host for this batch.
///
/// Checked against `staged`, never against the link's memory: the agent consumes a staged file,
/// so one it held for an earlier task is not there for this one.
pub(super) fn files_staged(tasks: &[Task], staged: &BTreeSet<String>) -> Result<(), String> {
    for task in tasks {
        if let Some(file) = task.files.iter().find(|f| !staged.contains(&f.blob)) {
            return Err(format!(
                "task '{}' stages the file {} under '{}', which was not put on the host for it",
                task.module, file.blob, file.arg
            ));
        }
    }
    Ok(())
}

/// What has to be true of the host before a batch goes out: it holds every payload the batch
/// names.
///
/// Called by [`super::batch::run_agent_batch`] and written over [`AgentChannel`] rather than over the link, so
/// the call itself can be tested: deleting it, or asking only when *every* task carries a
/// payload instead of when *any* does, is a change that otherwise leaves a green suite and ships
/// a batch the host cannot run.
///
/// A batch that names no payload asks nothing. A batch that names one asks for it once per link,
/// and is refused here rather than by the agent, where a payload nobody sent reads as the
/// module's own failure.
pub(super) async fn blob_preflight<C: AgentChannel>(
    link: &mut C,
    host: &str,
    tasks: &[Task],
    blob: Option<&Union>,
    files: &[(String, crate::action_plugins::FileBlob)],
    logs: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(union) = blob
        && tasks.iter().any(|task| task.payload.is_some())
    {
        ensure_blob(link, host, &union.hash, &union.zip_b64, logs).await?;
    }
    // The payloads before the files: a batch refused for its payload leaves nothing on the host,
    // and a staged file can hold a rendered secret.
    payloads_confirmed(tasks, link.memory())?;
    let staged = stage_files(link, host, files, logs).await?;
    files_staged(tasks, &staged)
}

/// What a batch reports when the payload it needs could not be put on the host.
///
/// Every task of it fails, carrying the storing error, and the batch ends as a **failure** rather
/// than as a transport error. The difference is the whole point: a transport error makes the
/// driver call the host unreachable and leave the run with no task line at all, which says
/// something false about a host whose native tasks would run perfectly well, and steps over the
/// `rescue:` and the `ignore_errors` an author wrote for exactly this.
pub(super) fn blob_failure(
    err: String,
    tasks: usize,
) -> (Vec<Option<TaskResult>>, Result<BatchOutcome, String>) {
    (
        vec![Some(TaskResult::failed_with(err)); tasks],
        Ok(BatchOutcome::Failed { at: 0 }),
    )
}
