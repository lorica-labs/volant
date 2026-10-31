// SPDX-License-Identifier: GPL-3.0-or-later
//! `[defaults] plugin_filters_cfg`: a YAML file whose `module_rejectlist` names modules the
//! reference then cannot resolve. Measured on ansible-core 2.19.12, a task naming one fails the
//! playbook while it is parsed, with the sentence a misspelt module gets, and only the name as the
//! list writes it is refused: `copy` in the list leaves `ansible.builtin.copy` running.

use std::path::Path;
use std::sync::RwLock;

static REJECTED: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Reads the modules the file at `path` rejects, under `module_rejectlist` or its older name
/// `module_blacklist`. A file that cannot be read or parsed rejects nothing.
pub(super) fn load(path: &Path) {
    let names: Vec<String> = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            let docs = crate::yaml::load_plain(&text, &path.display().to_string()).ok()?;
            crate::yaml::to_json(docs.first()?).ok()
        })
        .map(|doc| {
            ["module_rejectlist", "module_blacklist"]
                .iter()
                .filter_map(|key| doc.get(*key).and_then(serde_json::Value::as_array))
                .flatten()
                .filter_map(|name| name.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    *REJECTED.write().expect("filters lock") = names;
}

/// Whether the filter file rejects `module`, as the task writes it.
pub(crate) fn rejected(module: &str) -> bool {
    REJECTED
        .read()
        .expect("filters lock")
        .iter()
        .any(|name| name == module)
}
