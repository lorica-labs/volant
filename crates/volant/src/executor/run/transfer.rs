// SPDX-License-Identifier: GPL-3.0-or-later
//! A file of the host read back onto the controller a piece at a time, and what the transfers
//! of files hand a plugin.

use std::time::Duration;

use serde_json::{Map, Value};
use volant_protocol::{FromAgent, TaskResult, ToAgent};

use super::blob::PIECE_LEN;
use super::channel::AgentChannel;

/// Reads the host's file `remote` into the controller file `local`, a piece of at most
/// [`PIECE_LEN`] at a time, each within `patience`: the SHA-1 of what was written. `local` is
/// opened, not created, so whoever made it chose its place and its mode.
pub(in crate::executor) async fn get_file_within<C: AgentChannel>(
    link: &mut C,
    host: &str,
    remote: &str,
    local: &std::path::Path,
    logs: &mut Vec<String>,
    patience: Duration,
) -> Result<String, String> {
    use std::io::Write;
    let written = |err: std::io::Error| format!("writing {}: {err}", local.display());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(local)
        .map_err(written)?;
    let mut offset = 0;
    loop {
        let piece = async {
            let ask = ToAgent::ReadFile {
                path: remote.into(),
                offset,
                len: PIECE_LEN,
            };
            link.ask(&ask)
                .await
                .map_err(|err| format!("asking the agent for '{remote}': {err}"))?;
            let len = loop {
                match link.answer().await {
                    Ok(Some(FromAgent::FileBytes {
                        offset: at,
                        len,
                        error,
                        ..
                    })) if at == offset => match error {
                        None => break len,
                        Some(why) => return Err(why),
                    },
                    Ok(Some(FromAgent::Log { message, .. })) => {
                        logs.push(format!("[{host}] {message}"));
                    }
                    Ok(Some(FromAgent::Ready { .. })) => {}
                    Ok(Some(other)) => {
                        return Err(format!(
                            "the agent answered {other:?} while asked for '{remote}'"
                        ));
                    }
                    Ok(None) => {
                        return Err(format!("the agent stopped while sending '{remote}'"));
                    }
                    Err(err) => {
                        return Err(format!(
                            "reading the agent's answer about '{remote}': {err}"
                        ));
                    }
                }
            };
            if len == 0 {
                return Ok(Vec::new());
            }
            match link.answer_bytes().await {
                Ok(Some(bytes)) if bytes.len() as u64 == len => Ok(bytes),
                Ok(Some(bytes)) => Err(format!(
                    "the agent sent {} bytes of '{remote}' where it announced {len}",
                    bytes.len()
                )),
                Ok(None) => Err(format!("the agent stopped while sending '{remote}'")),
                Err(err) => Err(format!("reading '{remote}' from the agent: {err}")),
            }
        };
        let Ok(bytes) = tokio::time::timeout(patience, piece).await else {
            link.abandon();
            return Err(format!(
                "the agent did not send the next piece of '{remote}' within {} seconds",
                patience.as_secs()
            ));
        };
        let bytes = bytes?;
        if bytes.is_empty() {
            break;
        }
        file.write_all(&bytes).map_err(written)?;
        offset += bytes.len() as u64;
    }
    file.sync_all().map_err(written)?;
    drop(file);
    std::fs::File::open(local)
        .and_then(volant_protocol::encoding::sha1_hex_of)
        .map_err(written)
}

/// What a file put hands the plugin: `changed`, or the failure naming why the file is not there.
pub(in crate::executor) fn put_answer(put: Result<bool, String>) -> TaskResult {
    match put {
        Ok(changed) => {
            let mut done = Map::new();
            done.insert("changed".into(), Value::Bool(changed));
            TaskResult(done)
        }
        Err(why) => TaskResult::failed_with(why),
    }
}

/// What a file read back hands the plugin: the SHA-1 of what was written as `checksum`, or the
/// failure.
pub(in crate::executor) fn get_answer(got: Result<String, String>) -> TaskResult {
    match got {
        Ok(sum) => {
            let mut done = Map::new();
            done.insert("checksum".into(), Value::String(sum));
            TaskResult(done)
        }
        Err(why) => TaskResult::failed_with(why),
    }
}
