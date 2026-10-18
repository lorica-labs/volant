// SPDX-License-Identifier: GPL-3.0-or-later
//! The pieces of the bridged plugin that need no state: private files, command and failure replies.

use serde_json::{Map, Value, json};
use volant_protocol::TaskResult;

use super::super::Context;
use crate::bridge::Reply;

/// A vaulted controller `src` of `copy` or `template`, decrypted with the run's secrets into a
/// private directory under its own file name, and the path to hand the plugin. `copy` with
/// `decrypt: false` keeps the ciphertext. Secrets never reach the helper.
pub(super) fn decrypted_src(
    name: &str,
    ctx: &Context<'_>,
) -> Result<Option<(tempfile::TempDir, String)>, String> {
    let args = ctx.args;
    let Some(src) = args.get("src").and_then(Value::as_str) else {
        return Ok(None);
    };
    let kept = args.get("decrypt").and_then(Value::as_bool) == Some(false)
        || args.get("remote_src").and_then(Value::as_bool) == Some(true);
    let folder = match name {
        "copy" if !kept => "files",
        "template" => "templates",
        _ => return Ok(None),
    };
    let searched = super::super::files::search_paths(ctx.origin, ctx.playbook_dir, folder, src);
    let Some(found) = searched.iter().find(|p| p.is_file()) else {
        return Ok(None);
    };
    let unreadable =
        |err: &dyn std::fmt::Display| format!("could not read src={}: {err}", found.display());
    let bytes = std::fs::read(found).map_err(|e| unreadable(&e))?;
    if !crate::vault::is_encrypted(&bytes) {
        return Ok(None);
    }
    let plain =
        crate::vault::decrypt(&bytes, &crate::vault::run_secrets()).map_err(|e| unreadable(&e))?;
    let dir = tempfile::tempdir().map_err(|e| unreadable(&e))?;
    let path = dir.path().join(found.file_name().unwrap_or_default());
    write_private(&path, &plain).map_err(|e| unreadable(&e))?;
    Ok(Some((dir, path.display().to_string())))
}

pub(super) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(bytes)
}

/// `raw`'s arguments for a command line.
pub(super) fn raw(cmd: String) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("_raw_params".into(), Value::String(cmd));
    args
}

/// A path the controller made and quotes: absolute, one line, and nothing a shell reads inside
/// single quotes.
pub(super) fn plain_path(path: &str) -> bool {
    path.starts_with('/') && path.len() > 1 && !path.contains(['\'', '\n', '\r', '\0'])
}

/// A `raw` result read as the reply to a command: `{rc, stdout, stderr}`, or why there is none.
pub(super) fn command_reply(result: &TaskResult) -> Reply {
    let text = |key: &str| {
        result
            .0
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match result.0.get("rc").and_then(Value::as_i64) {
        Some(rc) => {
            Reply::Value(json!({"rc": rc, "stdout": text("stdout"), "stderr": text("stderr")}))
        }
        None => Reply::Error(
            result
                .0
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("the command ended without a return code")
                .to_string(),
        ),
    }
}

/// The reply to anything but a command: its failure's message, or `ok`.
pub(super) fn failure_or(result: &TaskResult, ok: Value) -> Reply {
    if result.failed() {
        Reply::Error(
            result
                .0
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("failed with no message")
                .to_string(),
        )
    } else {
        Reply::Value(ok)
    }
}
