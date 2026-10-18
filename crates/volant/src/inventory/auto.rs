// SPDX-License-Identifier: GPL-3.0-or-later
//! The `auto` inventory plugin of ansible-core 2.19.12: a YAML file whose `plugin:` key names the
//! plugin that reads it, and the config file reader that plugin's targets share.

use saphyr::{Scalar, Yaml};
use serde_json::Value;

use super::yaml::{Dialect, falsy};
use super::{Inventory, InventoryPlugin, PLUGINS, ParseCx, PluginError, Source};
use crate::yaml;

pub(super) struct Auto;

impl InventoryPlugin for Auto {
    fn name(&self) -> &'static str {
        "auto"
    }

    fn verify(&self, source: &Source) -> bool {
        super::sources::file_with_extension(source, &[Some("yml"), Some("yaml")])
    }

    fn parse(
        &self,
        source: &Source,
        into: &mut Inventory,
        cx: &ParseCx,
    ) -> Result<(), PluginError> {
        let path = source.text();
        // Only the `plugin` key is read: the rest of the file is the target plugin's to judge.
        let name = with_root(source, |root| {
            yaml::field(root, "plugin")
                .map(yaml::to_json)
                .transpose()
                .map_err(|e| e.to_string())
        })?;
        let name = match name {
            Some(name) if !falsy(&name) => name,
            _ => {
                return Err(failed(format!(
                    "no root 'plugin' key found, '{path}' is not a valid YAML inventory plugin \
                     config file"
                )));
            }
        };
        let Value::String(name) = &name else {
            return Err(failed(format!(
                "unsupported type {}",
                Dialect::Yaml.type_name(&name)
            )));
        };
        match parse_builtin(name, source, into, cx) {
            Some(result) => result,
            // A plugin of a collection runs in ansible-core, in the helper that starts only now.
            None if name.contains('.') => {
                let answer = cx
                    .helper()
                    .inventory(name, path)
                    .map_err(|e| failed(e.to_string()))?;
                super::collection::read(&answer, source, into, cx)
            }
            None => Err(failed(format!(
                "inventory config '{path}' specifies unknown plugin '{name}'"
            ))),
        }
    }
}

/// The builtin plugin a `plugin:` name stands for (`yaml` or `ansible.builtin.yaml`) reading
/// `source`, or `None` when the name is no builtin.
pub(super) fn parse_builtin(
    name: &str,
    source: &Source,
    into: &mut Inventory,
    cx: &ParseCx,
) -> Option<Result<(), PluginError>> {
    let path = source.text();
    let short = name.strip_prefix("ansible.builtin.").unwrap_or(name);
    let plugin = PLUGINS.iter().find(|p| p.name() == short)?;
    if short == "auto" {
        return Some(Err(failed("maximum recursion depth exceeded".to_string())));
    }
    if !plugin.verify(source) {
        return Some(Err(failed(format!(
            "inventory source '{path}' could not be verified by inventory plugin '{name}'"
        ))));
    }
    cx.verbose(
        1,
        format!(
            "Using inventory plugin 'ansible_collections.ansible.builtin.plugins.inventory.{short}' to process inventory source '{path}'"
        ),
    );
    Some(plugin.parse(source, into, cx))
}

/// An error that names no origin: the chain gives it the plugin and the source.
pub(super) fn failed(message: String) -> PluginError {
    PluginError {
        message,
        origin: None,
        cause: None,
    }
}

/// A plugin's config file as JSON, `null` when it is empty: `load_from_file`.
pub(super) fn load_config(source: &Source) -> Result<Value, PluginError> {
    with_root(source, |root| {
        yaml::to_json(root).map_err(|e| e.to_string())
    })
}

/// The one document of a plugin's config file, the null node when it is empty.
fn with_root<R>(
    source: &Source,
    read: impl FnOnce(&Yaml) -> Result<R, String>,
) -> Result<R, PluginError> {
    let bytes = std::fs::read(source.path()).map_err(|e| failed(e.to_string()))?;
    let text = String::from_utf8(bytes).map_err(|e| failed(e.to_string()))?;
    let docs = yaml::load(&text, source.text()).map_err(|e| failed(format!("{e:#}")))?;
    let null = Yaml::Value(Scalar::Null);
    let root = match docs.as_slice() {
        [] => &null,
        [one] => one,
        _ => {
            return Err(failed(
                "YAML parsing failed: Expected a single document in the stream but found \n                 another document."
                    .to_string(),
            ));
        }
    };
    read(root).map_err(failed)
}
