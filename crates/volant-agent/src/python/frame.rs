// SPDX-License-Identifier: GPL-3.0-or-later
//! What the parent's frames say, and the task results made from them.

use std::io;

use serde_json::Value;
use volant_protocol::TaskResult;

use super::Timing;
/// What an operator is shown of a module's own output when it is not a result.
const EXCERPT: usize = 2048;

/// Whether a frame is the parent saying it has imported the module_utils and is waiting.
pub(super) fn ready(frame: &[u8]) -> bool {
    serde_json::from_slice::<Value>(frame)
        .is_ok_and(|value| value.get("ready").and_then(Value::as_bool) == Some(true))
}

/// Whether the parent is saying that a package has moved under it since it started, and that it
/// has ended rather than run the request.
pub(super) fn stale(frame: &[u8]) -> bool {
    serde_json::from_slice::<Value>(frame)
        .is_ok_and(|value| value.get("stale").and_then(Value::as_bool) == Some(true))
}

/// The pid of the child the parent has just forked.
pub(super) fn started(frame: &[u8]) -> io::Result<u32> {
    serde_json::from_slice::<Value>(frame)
        .ok()
        .and_then(|value| value.get("started").and_then(Value::as_u64))
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the python server did not say which process it started",
            )
        })
}

/// The server's own measure of the task, in microseconds. A frame from a server that measured
/// nothing, or a value that is not a count, reads as unmeasured rather than as zero.
pub(super) fn timing(frame: &Value) -> Timing {
    let field = |name: &str| {
        frame
            .get("timing")
            .and_then(|timing| timing.get(name))
            .and_then(Value::as_u64)
    };
    Timing {
        fork: field("fork_us"),
        import: field("import_us"),
        module: field("module_us"),
    }
}

/// What the module did, as the task's result.
///
/// The module's own JSON is the truth whenever `stdout` is one object and nothing else, exit
/// status included: a module that says `failed: true` and exits non-zero is reporting its own
/// failure, which `TaskResult::failed` already reads. Everything else is a failure of this
/// engine's making and says so in words an operator can act on.
pub(super) fn result(frame: &Value) -> TaskResult {
    let stdout = frame.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = frame.get("stderr").and_then(Value::as_str).unwrap_or("");
    let said = if stderr.trim().is_empty() {
        None
    } else {
        Some(excerpt(stderr.trim()))
    };
    // A signal is never a result, whatever the module had printed by then: it did not finish, and
    // the commonest reason - the out-of-memory killer - leaves a perfectly parseable half.
    if let Some(signal) = frame.get("signal").and_then(Value::as_i64) {
        return fail(format!("the module was killed by signal {signal}"), said);
    }
    let code = frame.get("exit").and_then(Value::as_i64).unwrap_or(-1);
    // A result too large to carry is the module's own failure, said in the module's terms. The
    // parent read both pipes to the end either way, so the child is not left blocked on a full
    // one; what it did not keep is what would have made a frame the agent refuses, and a refused
    // frame reads as a dead server and poisons the rest of the batch.
    // The bound is the server's `STREAM_LIMIT`, sent in the frame by the side that enforces it.
    if frame.get("truncated").and_then(Value::as_bool) == Some(true) {
        let limit = frame.get("limit").and_then(Value::as_u64).map_or_else(
            || "the server's limit".to_string(),
            |limit| format!("{limit} bytes"),
        );
        return fail(
            format!(
                "the module wrote more than {limit}, which is more than a result can carry (exit status {code})"
            ),
            said,
        );
    }
    if stdout.trim().is_empty() {
        return fail(
            format!("the module wrote no result (exit status {code})"),
            said,
        );
    }
    match serde_json::from_str::<Value>(stdout) {
        Ok(Value::Object(result)) => crate::modules::module_result(result),
        _ => fail(
            format!(
                "the module wrote something that is not a result (exit status {code}): {}",
                excerpt(stdout.trim())
            ),
            said,
        ),
    }
}

/// A failed task in Ansible's shape, with what the module wrote to stderr behind the sentence
/// when it wrote anything: a traceback is the one thing an operator needs and the one thing a
/// result object cannot carry.
pub(super) fn fail(msg: impl Into<String>, stderr: Option<String>) -> TaskResult {
    let msg = match stderr {
        Some(said) => format!("{}: {said}", msg.into()),
        None => msg.into(),
    };
    TaskResult::failed_with(msg)
}

/// The first [`EXCERPT`] bytes of `text`, cut on a character boundary.
pub(super) fn excerpt(text: &str) -> String {
    if text.len() <= EXCERPT {
        return text.to_string();
    }
    let mut end = EXCERPT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}
