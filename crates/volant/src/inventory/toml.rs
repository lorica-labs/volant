// SPDX-License-Identifier: GPL-3.0-or-later
//! The `toml` inventory plugin: the same groups as the `yaml` one, written as tables, read with
//! the TOML 1.0 grammar `tomllib` takes.

use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item};

use super::yaml::{Dialect, falsy, read_groups};
use super::{Inventory, InventoryPlugin, ParseCx, PluginError, Source};

pub(super) struct Toml;

impl InventoryPlugin for Toml {
    fn name(&self) -> &'static str {
        "toml"
    }

    fn verify(&self, source: &Source) -> bool {
        super::sources::file_with_extension(source, &[Some("toml")])
    }

    fn parse(
        &self,
        source: &Source,
        into: &mut Inventory,
        cx: &ParseCx,
    ) -> Result<(), PluginError> {
        let failed = |message: String| PluginError {
            message,
            origin: None,
            cause: None,
        };
        let bytes = std::fs::read(source.path()).map_err(|e| {
            failed(format!(
                "An error occurred while trying to read the file '{}': {e}",
                source.text()
            ))
        })?;
        let text = String::from_utf8(bytes).map_err(|e| failed(e.to_string()))?;
        read_text(&text, into, cx, source.text()).map_err(failed)
    }
}

/// TOML text read into `inv`; `path` is how an error names the file.
pub(super) fn read_text(
    text: &str,
    inv: &mut Inventory,
    cx: &ParseCx,
    path: &str,
) -> Result<(), String> {
    let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        let at = e.span().map_or(0, |s| s.start).min(text.len());
        let before = &text[..text.floor_char_boundary(at)];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        format!(
            "TOML file '{path}' is invalid: {} (at line {line}, column {column})",
            e.message()
        )
    })?;
    let data = table(doc.iter());
    if data.as_object().is_some_and(Map::is_empty) {
        return Err("Parsed empty TOML file".to_string());
    }
    if data.get("plugin").is_some_and(|p| !falsy(p)) {
        return Err("Plugin configuration TOML file, not TOML inventory".to_string());
    }
    read_groups(&data, Dialect::Toml, inv, None, cx)
}

fn table<'a>(entries: impl Iterator<Item = (&'a str, &'a Item)>) -> Value {
    Value::Object(
        entries
            .map(|(key, item)| (key.to_string(), item_value(item)))
            .collect::<Map<_, _>>(),
    )
}

fn item_value(item: &Item) -> Value {
    match item {
        Item::None => Value::Null,
        Item::Value(v) => value(v),
        Item::Table(t) => table(t.iter()),
        Item::ArrayOfTables(a) => Value::Array(a.iter().map(|t| table(t.iter())).collect()),
    }
}

fn value(v: &toml_edit::Value) -> Value {
    use toml_edit::Value as T;
    match v {
        T::String(s) => Value::String(s.value().clone()),
        T::Integer(i) => Value::from(*i.value()),
        T::Float(f) => serde_json::Number::from_f64(*f.value()).map_or(Value::Null, Value::Number),
        T::Boolean(b) => Value::Bool(*b.value()),
        T::Datetime(d) => Value::String(d.value().to_string()),
        T::Array(a) => Value::Array(a.iter().map(value).collect()),
        T::InlineTable(t) => Value::Object(
            t.iter()
                .map(|(key, v)| (key.to_string(), value(v)))
                .collect(),
        ),
    }
}
