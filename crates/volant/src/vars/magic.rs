// SPDX-License-Identifier: GPL-3.0-or-later
//! The magic variables Ansible defines for every host.

use std::path::Path;

use serde_json::{Map, Value};

use super::{VarStore, omit_token};
/// An inventory name without its domain, which is what `inventory_hostname_short` reports.
pub(super) fn short_name(host: &str) -> String {
    host.split('.').next().unwrap_or(host).to_string()
}

impl VarStore {
    /// The magic variables that belong to one host. The inventory-wide ones are not here: see
    /// `shared_values`.
    pub(super) fn magic(&self, host: &str) -> Map<String, Value> {
        let mut vars = Map::new();
        let short = short_name(host);
        let mut group_names = self.group_names.get(host).cloned().unwrap_or_default();
        group_names.sort();
        vars.insert(
            "inventory_hostname".to_string(),
            Value::String(host.to_string()),
        );
        vars.insert("inventory_hostname_short".to_string(), Value::String(short));
        vars.insert(
            "group_names".to_string(),
            serde_json::to_value(group_names).unwrap_or_default(),
        );
        vars.insert(
            "playbook_dir".to_string(),
            Value::String(self.playbook_dir.display().to_string()),
        );
        // The file the host was read from, as the reference records it on the host.
        let own = self.dynamic.source_of(host);
        let file = own.or(self.inventory_file.as_deref());
        let dir = own.and_then(Path::parent).or(self.inventory_dir.as_deref());
        if let Some(dir) = dir {
            vars.insert(
                "inventory_dir".to_string(),
                Value::String(dir.display().to_string()),
            );
        }
        if let Some(file) = file {
            vars.insert(
                "inventory_file".to_string(),
                Value::String(file.display().to_string()),
            );
        }
        vars.insert("omit".to_string(), Value::String(omit_token().to_string()));
        vars.insert(
            "ansible_check_mode".to_string(),
            Value::Bool(self.check_mode),
        );
        vars.insert("ansible_diff_mode".to_string(), Value::Bool(self.diff_mode));
        vars.insert("ansible_forks".to_string(), Value::from(self.forks));
        vars.insert(
            "ansible_run_tags".to_string(),
            Value::from(self.run_tags.clone()),
        );
        vars.insert(
            "ansible_skip_tags".to_string(),
            Value::from(self.skip_tags.clone()),
        );
        vars.insert("ansible_version".to_string(), ansible_version());
        vars.insert(
            "volant_version".to_string(),
            Value::String(env!("CARGO_PKG_VERSION").to_string()),
        );
        vars
    }
}

/// The ansible-core release Volant reproduces, as the `ansible_version` dictionary roles
/// consult. The number lives in tests/golden/ANSIBLE_VERSION and nowhere else.
pub fn ansible_version() -> Value {
    let full = include_str!("../../tests/golden/ANSIBLE_VERSION").trim();
    let mut parts = full.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let (major, minor, revision) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    serde_json::json!({ "full": full, "major": major, "minor": minor, "revision": revision, "string": full })
}
