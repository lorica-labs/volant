// SPDX-License-Identifier: GPL-3.0-or-later
//! The `volant playbook` arguments of a recorded run, read from the options ansible-core parsed.

use clap::Parser as _;
use serde_json::Value;

use super::recording::Header;
use super::relative;
use crate::cli::PlaybookArgs;

/// The recorded run's options as `volant playbook` arguments, its files taken from the
/// recording's working directory as they are from this one.
pub(super) fn playbook_args(header: &Header) -> Result<PlaybookArgs, String> {
    let cli = &header.cli;
    let strings = |key: &str| -> Vec<String> {
        cli.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let text = |key: &str| {
        cli.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let flag = |key: &str| cli.get(key).and_then(Value::as_bool).unwrap_or(false);
    let local = |path: &str| relative(path, &header.cwd).to_string_lossy().into_owned();

    let mut argv = vec!["volant-playbook".to_owned()];
    let mut pair = |option: &str, value: String| argv.extend([option.to_owned(), value]);
    for source in strings("inventory") {
        pair("--inventory", local(&source));
    }
    for extra in strings("extra_vars") {
        pair("--extra-vars", extra);
    }
    let tags = strings("tags");
    if !tags.is_empty() && tags != ["all"] {
        pair("--tags", tags.join(","));
    }
    let skip = strings("skip_tags");
    if !skip.is_empty() {
        pair("--skip-tags", skip.join(","));
    }
    for (key, option) in [
        ("subset", "--limit"),
        ("remote_user", "--user"),
        ("connection", "--connection"),
        ("become_user", "--become-user"),
        ("become_method", "--become-method"),
        ("start_at_task", "--start-at-task"),
    ] {
        if let Some(value) = text(key) {
            pair(option, value.to_owned());
        }
    }
    if let Some(forks) = cli.get("forks").and_then(Value::as_u64) {
        pair("--forks", forks.to_string());
    }
    for id in strings("vault_ids") {
        pair("--vault-id", id);
    }
    for file in strings("vault_password_files") {
        pair("--vault-password-file", local(&file));
    }
    for (key, option) in [
        ("check", "--check"),
        ("diff", "--diff"),
        ("become", "--become"),
        ("force_handlers", "--force-handlers"),
        ("flush_cache", "--flush-cache"),
        ("ask_vault_pass", "--ask-vault-pass"),
    ] {
        if flag(key) {
            argv.push(option.to_owned());
        }
    }
    let verbosity = cli.get("verbosity").and_then(Value::as_u64).unwrap_or(0);
    if verbosity > 0 {
        argv.push(format!("-{}", "v".repeat(verbosity as usize)));
    }
    argv.extend(strings("args").iter().map(|p| local(p)));
    PlaybookArgs::try_parse_from(argv)
        .map_err(|e| format!("the recorded options are not ones volant playbook takes: {e}"))
}
