// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading a recording the `volant.compat.record` callback wrote: JSON Lines, gzip when the name
//! ends in `.gz`, possibly cut short by a run that was killed.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

/// The newest recording format this Volant reads.
pub(crate) const FORMAT: u64 = 1;

/// A recording as read: its header, every complete event after it, and whether the file ended
/// in the middle of a line or of its compressed stream.
#[derive(Debug)]
pub(crate) struct Recording {
    pub(crate) header: Header,
    /// Every line after the header, each an object with an `event` key.
    pub(crate) events: Vec<Value>,
    pub(crate) truncated: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Header {
    pub(crate) ansible_core: String,
    /// The run's options as ansible-core parsed them (`context.CLIARGS`).
    pub(crate) cli: Map<String, Value>,
    pub(crate) cwd: String,
    pub(crate) playbooks: Vec<Hashed>,
    pub(crate) inventory: InventorySources,
    pub(crate) extra_vars_hash: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Hashed {
    pub(crate) path: String,
    pub(crate) sha256: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct InventorySources {
    pub(crate) files: Vec<Hashed>,
}

/// Reads the recording at `path`.
pub(crate) fn read(path: &Path) -> Result<Recording, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let (text, cut) = if path.extension().is_some_and(|e| e == "gz") {
        gunzip(&bytes).map_err(|e| format!("{}: {e}", path.display()))?
    } else {
        (bytes, false)
    };
    parse(&text, cut).map_err(|e| format!("{}: {e}", path.display()))
}

/// The recording in `text`, `cut` when its compressed stream ended early.
pub(super) fn parse(text: &[u8], cut: bool) -> Result<Recording, String> {
    let text = String::from_utf8_lossy(text);
    let complete = text.rfind('\n').map_or("", |end| &text[..end]);
    let truncated = cut || !text.is_empty() && !text.ends_with('\n');
    let mut lines = complete
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty());
    let (_, first) = lines.next().ok_or("the recording is empty")?;
    let head: Value =
        serde_json::from_str(first).map_err(|e| format!("line 1 is not JSON: {e}"))?;
    if head.get("event").and_then(Value::as_str) != Some("header") {
        return Err("the first line is not a header".to_owned());
    }
    // Read before the rest of the header: a newer format may have moved any other field.
    let format = head.get("format").and_then(Value::as_u64).unwrap_or(0);
    if format > FORMAT {
        return Err(format!(
            "the recording is format {format}, and this Volant reads format {FORMAT} at most"
        ));
    }
    let header: Header =
        serde_json::from_value(head).map_err(|e| format!("line 1 is not a header: {e}"))?;
    let events = lines
        .map(|(n, line)| {
            let event: Value = serde_json::from_str(line)
                .map_err(|e| format!("line {} is not JSON: {e}", n + 1))?;
            match event.get("event") {
                Some(Value::String(_)) => Ok(event),
                _ => Err(format!("line {} has no event", n + 1)),
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(Recording {
        header,
        events,
        truncated,
    })
}

/// Decompresses one gzip member, and says whether it was cut short: what decompressed before
/// the end is kept, since a killed run leaves an unfinished stream. The CRC is not checked; a
/// damaged stream shows as a line that is not JSON.
fn gunzip(bytes: &[u8]) -> Result<(Vec<u8>, bool), String> {
    const FHCRC: u8 = 2;
    const FEXTRA: u8 = 4;
    const FNAME: u8 = 8;
    const FCOMMENT: u8 = 16;
    if bytes.len() < 10 || bytes[..3] != [0x1f, 0x8b, 8] {
        return Err("not a gzip file".to_owned());
    }
    let flags = bytes[3];
    let mut at = 10;
    if flags & FEXTRA != 0 {
        let len = bytes.get(at..at + 2).ok_or("gzip header cut short")?;
        at += 2 + usize::from(u16::from_le_bytes([len[0], len[1]]));
    }
    for flag in [FNAME, FCOMMENT] {
        if flags & flag != 0 {
            let end = bytes
                .get(at..)
                .and_then(|rest| rest.iter().position(|&b| b == 0));
            at += end.ok_or("gzip header cut short")? + 1;
        }
    }
    if flags & FHCRC != 0 {
        at += 2;
    }
    let body = bytes.get(at..).ok_or("gzip header cut short")?;
    match miniz_oxide::inflate::decompress_to_vec(body) {
        Ok(out) => Ok((out, false)),
        Err(err) if err.status == miniz_oxide::inflate::TINFLStatus::FailedCannotMakeProgress => {
            Ok((err.output, true))
        }
        Err(err) => Err(format!("cannot decompress: {err}")),
    }
}

#[cfg(test)]
#[path = "recording_tests.rs"]
mod tests;
