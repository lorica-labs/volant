// SPDX-License-Identifier: GPL-3.0-or-later
//! The `inventory` command: what `ansible-inventory --list` and `--host` print.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Args;
use serde::Serialize;
use serde_json::Value;

use super::announce::{announce, inventory_sources};
use crate::bridge::Bridge;
use crate::config::Config;
use crate::inventory::{GraphItem, Inventory, Note, load_sources_with};
use crate::render::Renderer;
use crate::stats::error_code;
use crate::vars::VarStore;

/// The arguments of `ansible-inventory` this release reads. `--yaml`, `--toml` and `--export` are not here, so clap refuses them by name.
#[derive(Args, Debug)]
pub struct InventoryArgs {
    /// Print the whole inventory as JSON.
    #[arg(long, overrides_with = "list")]
    list: bool,
    /// Print the variables of one host as JSON.
    #[arg(long, value_name = "HOST", overrides_with = "host")]
    host: Option<String>,
    /// Print the group tree, the whole inventory or one group.
    #[arg(long, value_name = "GROUP", num_args = 0..=1, overrides_with = "graph")]
    graph: Option<Option<String>>,
    /// With `--graph`, add the variables of every host and group. The other actions ignore it.
    #[arg(long)]
    vars: bool,
    /// Inventory source: a file, a directory, or a comma-separated host list. Repeatable; the
    /// sources are merged in order.
    #[arg(
        short = 'i',
        long = "inventory",
        alias = "inventory-file",
        value_name = "SOURCE"
    )]
    inventory: Vec<String>,
    /// Limit `--list` to the hosts matching this pattern.
    #[arg(short = 'l', long = "limit", value_name = "SUBSET")]
    limit: Option<String>,
    /// Extra variables: `key=value` pairs, inline JSON or YAML, or `@file`. Repeatable.
    #[arg(short = 'e', long = "extra-vars", value_name = "VARS")]
    extra_vars: Vec<String>,
    /// Write the document to this file instead of standard output.
    #[arg(long, value_name = "OUTPUT_FILE")]
    output: Option<PathBuf>,
    /// Show the configuration file and the version. Repeatable.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,
    /// Ask for the vault password on the terminal.
    #[arg(
        short = 'J',
        long = "ask-vault-pass",
        visible_alias = "ask-vault-password"
    )]
    ask_vault_pass: bool,
    /// File, or executable printing the password, for the vault. Repeatable.
    #[arg(
        long = "vault-password-file",
        alias = "vault-pass-file",
        value_name = "VAULT_PASSWORD_FILE"
    )]
    vault_password_file: Vec<PathBuf>,
    /// Vault identity, `label@source` where the source is a file, an executable or `prompt`.
    /// Repeatable.
    #[arg(long = "vault-id", value_name = "VAULT_IDS")]
    vault_id: Vec<String>,
}

/// Runs the command and returns the process exit code.
pub fn inventory(args: InventoryArgs) -> i32 {
    match execute(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("[ERROR]: {err:#}");
            error_code(&err)
        }
    }
}

fn execute(args: &InventoryArgs) -> anyhow::Result<i32> {
    let chosen = usize::from(args.list)
        + usize::from(args.host.is_some())
        + usize::from(args.graph.is_some());
    if chosen == 0 {
        eprintln!(
            "[ERROR]: No action selected, at least one of --host, --graph or --list needs to be specified."
        );
        return Ok(5);
    }
    if chosen > 1 {
        eprintln!(
            "[ERROR]: Conflicting options used, only one of --host, --graph or --list can be used at the same time."
        );
        return Ok(5);
    }
    let config = crate::runtime::resolve::configured(Config::load()?);
    let mut out = Renderer::new(config.color_choice(false), args.verbose);
    announce(&mut out, None);
    let secrets = match crate::vault::load(&crate::vault::Request {
        ask: args.ask_vault_pass,
        password_files: args.vault_password_file.clone(),
        ids: args.vault_id.clone(),
        config_file: config.vault_password_file.clone(),
        config_ids: config.vault_identity_list.clone(),
    }) {
        Ok(secrets) => secrets,
        Err(err) => {
            eprintln!("[WARNING]: {}", err.warning);
            eprintln!("[ERROR]: {}", err.message);
            return Ok(1);
        }
    };
    crate::vault::install(secrets);
    let loaded = match load(&args.inventory, &config, &mut out) {
        Ok(inventory) => inventory,
        Err(code) => return Ok(code),
    };
    let base = std::env::current_dir()?;
    let extra = crate::vars::parse_extra_vars(&args.extra_vars, &base)?;
    // The reference reads `group_vars` and `host_vars` beside each source and in the directory
    // it was started from.
    let store = VarStore::with_sources(
        &loaded,
        &inventory_sources(&args.inventory, &config),
        &base,
        extra,
    )?;
    let vars_of =
        |host: &str| -> BTreeMap<String, Value> { store.listed_vars(host).into_iter().collect() };
    if let Some(group) = &args.graph {
        return graph(args, group.as_deref().unwrap_or("all"), &loaded, &store);
    }
    let document = if let Some(name) = &args.host {
        // The reference leaves `--limit` out of `--host`.
        let resolution = loaded.resolve(name);
        if let Some(bad) = &resolution.invalid {
            anyhow::bail!("Invalid host list pattern: {bad}");
        }
        for unmatched in &resolution.unmatched {
            out.warning(
                &format!("Could not match supplied host pattern, ignoring: {unmatched}"),
                false,
            );
        }
        let [host] = resolution.hosts.as_slice() else {
            eprintln!("[ERROR]: You must pass a single valid host to --host parameter");
            return Ok(5);
        };
        loaded.host_dump(&host.name, &vars_of)
    } else {
        let limited = match &args.limit {
            Some(pattern) => match limit(&loaded, pattern, &mut out) {
                Ok(inventory) => inventory,
                Err(code) => return Ok(code),
            },
            None => loaded,
        };
        limited.dump_with(&vars_of)
    };
    let text = to_json(&document)?;
    match &args.output {
        // The reference writes the text as it is, without the newline `print` adds.
        Some(path) => std::fs::write(path, text).map_err(|err| write_failure(path, &err))?,
        None => println!("{text}"),
    }
    Ok(0)
}

/// `--graph`: the tree to standard output or `--output`. `--limit` and `--yaml` leave it as it is.
fn graph(
    args: &InventoryArgs,
    root: &str,
    inventory: &Inventory,
    store: &VarStore,
) -> anyhow::Result<i32> {
    let mut lines = |item: GraphItem<'_>| -> Result<Vec<String>, String> {
        if !args.vars {
            return Ok(Vec::new());
        }
        let vars: BTreeMap<String, Value> = match item {
            GraphItem::Host(name) => store.listed_vars(name).into_iter().collect(),
            GraphItem::Group(name) => store
                .listed_group_vars(name, &inventory.group_vars_of(name).into_iter().collect())
                .into_iter()
                .collect(),
        };
        vars.iter()
            .map(|(name, value)| Ok(format!("{{{name} = {}}}", shown(value)?)))
            .collect()
    };
    let text = match inventory.graph(root, &mut lines) {
        Ok(Some(text)) => text,
        Ok(None) => {
            eprintln!("[ERROR]: Pattern must be valid group name when using --graph");
            return Ok(5);
        }
        Err(failure) => {
            // The reference words it without the task-style lead.
            let message = failure
                .strip_prefix(crate::vault::UNDECRYPTABLE)
                .unwrap_or(&failure);
            match crate::vault::failure_origin(&failure) {
                Some(o) => eprintln!(
                    "[ERROR]: {message}\n{}\n",
                    crate::render::source_context(&o)
                ),
                None => eprintln!("[ERROR]: {message}"),
            }
            return Ok(1);
        }
    };
    match &args.output {
        // The tree ends with the newline of its last line, and `print` adds none after it.
        Some(path) => {
            std::fs::write(path, text.trim_end_matches('\n'))
                .map_err(|err| write_failure(path, &err))?;
        }
        None => print!("{text}"),
    }
    Ok(0)
}

/// Python's `str()` of a value as `--graph --vars` shows it. A `!vault` value that is the
/// variable itself is decrypted, with the failure's message when it cannot be; one inside a list
/// or a mapping is left as the reference's `repr` shows it, `EncryptedString(ciphertext=...)`.
fn shown(value: &Value) -> Result<String, String> {
    if let Value::String(text) = value {
        if let Some(cipher) = crate::yaml::vault_text(text) {
            return crate::vault::reveal(&cipher)
                .map_err(|reason| crate::vault::undecryptable(&cipher, &reason));
        }
        if let Some(plain) = crate::yaml::unsafe_text(text) {
            return Ok(plain);
        }
        return Ok(text.clone());
    }
    Ok(nested(value))
}

/// Python's `repr` of a value inside a container.
fn nested(value: &Value) -> String {
    let quoted = |text: &str| crate::playbook::python_repr(&Value::String(text.to_string()));
    match value {
        Value::String(text) => {
            if let Some(cipher) = crate::yaml::vault_text(text) {
                format!("EncryptedString(ciphertext={})", quoted(&cipher))
            } else if let Some(plain) = crate::yaml::unsafe_text(text) {
                quoted(&plain)
            } else {
                quoted(text)
            }
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(nested).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", quoted(k), nested(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => crate::playbook::python_repr(other),
    }
}

/// The inventory every `-i` names, else the configuration's, else the reference's default; what
/// is said while reading goes to the terminal as `ansible-playbook` says it. `Err` carries the
/// exit code, with the message already printed.
pub(super) fn load(
    given: &[String],
    config: &Config,
    out: &mut Renderer,
) -> Result<Inventory, i32> {
    let sources = inventory_sources(given, config);
    let bridge = Bridge::late();
    let loaded = load_sources_with(&sources, &config.inventory_plugins, &|| Arc::clone(&bridge));
    for note in &loaded.notes {
        match note {
            Note::Verbose(level, text) => out.verbose(*level, text),
            Note::Warning(text) => out.warning(text, false),
            Note::Error(text) => eprintln!("[ERROR]: {text}"),
            // A source's note ends with the newline the block needs; the printer adds its own.
            Note::Deprecation(text) => {
                crate::playbook::print_deprecation(text.strip_suffix('\n').unwrap_or(text));
            }
        }
    }
    if let Some(fatal) = loaded.error {
        eprintln!("[ERROR]: {}", fatal.message);
        return Err(fatal.code);
    }
    Ok(loaded.inventory)
}

/// The inventory narrowed to the hosts `pattern` matches, groups kept. A pattern nothing matches
/// is a warning and an inventory with no host, as in the reference; one that is not a pattern is
/// exit 1.
pub(super) fn limit(
    inventory: &Inventory,
    pattern: &str,
    out: &mut Renderer,
) -> Result<Inventory, i32> {
    let resolution = inventory.resolve(pattern);
    if let Some(bad) = &resolution.invalid {
        eprintln!("[ERROR]: Invalid host list pattern: {bad}");
        return Err(1);
    }
    for warning in &resolution.warnings {
        out.warning(warning, false);
    }
    for unmatched in &resolution.unmatched {
        out.warning(
            &format!("Could not match supplied host pattern, ignoring: {unmatched}"),
            false,
        );
    }
    let keep: HashSet<String> = resolution.hosts.into_iter().map(|h| h.name).collect();
    Ok(inventory.limited_to(&keep))
}

/// The reference's words for an output file it cannot write, Python's `OSError` text included.
fn write_failure(path: &Path, err: &std::io::Error) -> anyhow::Error {
    let reason = err.to_string();
    let reason = reason.split(" (os error").next().unwrap_or(&reason);
    let code = err.raw_os_error().unwrap_or(0);
    let shown = path.display();
    anyhow::anyhow!(
        "Unable to write to destination file '{shown}': [Errno {code}] {reason}: b'{shown}'"
    )
}

/// `json.dumps(value, indent=4, sort_keys=True, ensure_ascii=False)`.
fn to_json(value: &Value) -> anyhow::Result<String> {
    let mut buffer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
    sorted(value).serialize(&mut serializer)?;
    Ok(String::from_utf8(buffer)?)
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted(&map[k])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_four_spaces_sorted_and_not_escaped() {
        let value = serde_json::json!({"b": [1, {"z": null, "a": "café"}], "a": {}});
        assert_eq!(
            to_json(&value).unwrap(),
            "{\n    \"a\": {},\n    \"b\": [\n        1,\n        {\n            \"a\": \"café\",\n            \"z\": null\n        }\n    ]\n}"
        );
    }
}
