// SPDX-License-Identifier: GPL-3.0-or-later
//! One request to a helper and its answer, read frame by frame.

use std::fmt;
use std::time::Instant;

use serde_json::{Value, json};
use volant_protocol::frame::MAX_FRAME_LEN;

use super::{Ask, BridgeError, Reply};
use crate::python::{ActionHelper, PythonBuilder, Silence};

/// One action request, the asks it makes answered in turn, until its value or its error.
pub(super) fn converse(
    helper: &mut ActionHelper,
    id: u64,
    name: &str,
    request: &str,
    deadline: Instant,
    asks: &mut dyn FnMut(Ask) -> Reply,
) -> Result<Value, BridgeError> {
    helper
        .send(request.as_bytes())
        .map_err(|err| exited(&err))?;
    loop {
        let frame = match helper.next(deadline) {
            Ok(Ok(Some(frame))) => frame,
            Ok(Ok(None)) | Err(Silence::Gone) => {
                return Err(exited(&"it closed its output before answering"));
            }
            Ok(Err(err)) => return Err(unreadable(&err)),
            Err(Silence::Expired) => {
                return Err(BridgeError::Helper(format!(
                    "the action plugin '{name}' was still running at the task's deadline"
                )));
            }
        };
        let message = decode(&frame, id)?;
        let Some(question) = message.get("ask") else {
            return outcome(message);
        };
        let ask = Ask::read(question).map_err(|why| {
            BridgeError::Helper(format!(
                "the Python helper asked something out of shape: {why}"
            ))
        })?;
        let reply = json!({"id": id, "reply": asks(ask).to_json()}).to_string();
        // An answer the helper could not read becomes an error the plugin can.
        let reply = if reply.len() > MAX_FRAME_LEN {
            json!({"id": id, "reply": {"error": format!(
                "the answer is {} bytes, over the {MAX_FRAME_LEN} byte limit",
                reply.len()
            )}})
            .to_string()
        } else {
            reply
        };
        helper.send(reply.as_bytes()).map_err(|err| exited(&err))?;
    }
}

fn exited(detail: &dyn fmt::Display) -> BridgeError {
    BridgeError::Helper(format!("the Python helper exited: {detail}"))
}

/// A read that failed: a frame over the cap is the helper breaking the protocol.
fn unreadable(err: &std::io::Error) -> BridgeError {
    if err.kind() == std::io::ErrorKind::InvalidData {
        BridgeError::Helper(format!("the Python helper broke the protocol: {err}"))
    } else {
        exited(err)
    }
}

pub(super) fn defaults_shape() -> BridgeError {
    BridgeError::Helper(
        "the Python helper answered the module_defaults names in a shape it should not".into(),
    )
}

/// One request and its answer, checked to be the answer to that request.
pub(super) fn answer(
    helper: &mut PythonBuilder,
    id: u64,
    request: &[u8],
) -> Result<Value, BridgeError> {
    let frame = match helper.round_trip(request) {
        Ok(Some(frame)) => frame,
        Ok(None) => return Err(exited(&"it closed its output before answering")),
        Err(err) => return Err(unreadable(&err)),
    };
    outcome(decode(&frame, id)?)
}

/// A frame read as the JSON message of request `id`.
fn decode(frame: &[u8], id: u64) -> Result<Value, BridgeError> {
    let text = std::str::from_utf8(frame).map_err(|err| {
        BridgeError::Helper(format!(
            "the Python helper answered bytes that are not UTF-8: {err}"
        ))
    })?;
    let reply: Value = serde_json::from_str(text).map_err(|err| {
        BridgeError::Helper(format!(
            "the Python helper answered something that is not JSON: {err}"
        ))
    })?;
    if reply.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(BridgeError::Helper(format!(
            "the Python helper answered request {} to request {id}",
            reply
                .get("id")
                .map_or("none".to_string(), ToString::to_string)
        )));
    }
    Ok(reply)
}

/// The value a final message carries, or its error.
fn outcome(mut reply: Value) -> Result<Value, BridgeError> {
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no message")
            .to_string();
        return Err(match error.get("kind").and_then(Value::as_str) {
            Some("plugin") => BridgeError::Plugin(message),
            Some("not_found") => BridgeError::NotFound(message),
            other => BridgeError::Helper(format!(
                "the Python helper reported an error of kind {other:?}: {message}"
            )),
        });
    }
    match reply.get_mut("value") {
        Some(value) => Ok(value.take()),
        None => Err(BridgeError::Helper(
            "the Python helper's answer carries neither a value nor an error".into(),
        )),
    }
}
