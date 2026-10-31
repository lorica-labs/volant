// SPDX-License-Identifier: GPL-3.0-or-later
//! The filters and tests the playbook's own plugin directories define, which win over the
//! engine's of the same name.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::PoisonError;

use serde_json::Value;

use super::{Bridge, PluginKind};

/// The plugin directories last asked about, and the names they define.
pub(super) type Own = Option<(Vec<PathBuf>, HashSet<(PluginKind, String)>)>;

impl Bridge {
    /// Whether a plugin directory of the playbook's own, or a configured one, defines a filter or
    /// a test of this short `name`: measured on ansible-core 2.19.12, a `filter_plugins/core.py`
    /// that defines `flatten` is the `flatten` a template calls. The helper is asked only when
    /// such a directory is there.
    pub fn overrides(&self, kind: PluginKind, name: &str) -> bool {
        if name.contains('.') || kind == PluginKind::Lookup {
            return false;
        }
        let dirs = self
            .dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let beside = dirs.iter().any(|d| {
            ["filter_plugins", "test_plugins"]
                .iter()
                .any(|s| d.join(s).is_dir())
        });
        let configured = ["ANSIBLE_FILTER_PLUGINS", "ANSIBLE_TEST_PLUGINS"]
            .iter()
            .any(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()));
        if !beside && !configured {
            return false;
        }
        let mut own = self.own.lock().unwrap_or_else(PoisonError::into_inner);
        if own.as_ref().is_none_or(|(asked, _)| *asked != dirs) {
            let answer = self
                .defaults_request("own_plugins", &[])
                .unwrap_or_default();
            let names = [(PluginKind::Filter, "filter"), (PluginKind::Test, "test")]
                .into_iter()
                .flat_map(|(kind, key)| {
                    let listed = answer.get(key).and_then(Value::as_array).cloned();
                    listed
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(move |n| n.as_str().map(|n| (kind, n.to_string())))
                })
                .collect();
            *own = Some((dirs, names));
        }
        own.as_ref()
            .is_some_and(|(_, names)| names.contains(&(kind, name.to_string())))
    }
}
