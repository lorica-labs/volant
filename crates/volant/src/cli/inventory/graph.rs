// SPDX-License-Identifier: GPL-3.0-or-later
//! `--graph`: the tree of groups and hosts, and the variables `--vars` adds to it.

use super::*;

/// `--graph`: the tree to standard output or `--output`. `--limit` and `--yaml` leave it as it is;
/// with `--export`, a host's lines are its own variables only.
pub(super) fn graph(
    args: &InventoryArgs,
    root: &str,
    inventory: &Inventory,
    store: &VarStore,
    export: bool,
) -> anyhow::Result<i32> {
    let mut lines = |item: GraphItem<'_>| -> Result<Vec<String>, String> {
        if !args.vars {
            return Ok(Vec::new());
        }
        let vars = match item {
            GraphItem::Host(name) if export => store.exported_vars(name),
            GraphItem::Host(name) => store.listed_vars(name),
            GraphItem::Group(name) => {
                store.listed_group_vars(name, &inventory.group_vars_of(name).into_iter().collect())
            }
        };
        let vars: BTreeMap<String, Value> = written(vars).into_iter().collect();
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
