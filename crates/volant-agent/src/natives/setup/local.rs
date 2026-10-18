// SPDX-License-Identifier: GPL-3.0-or-later
//! The `local` collector: `{}` when the fact path holds no `*.fact` file. A fact file is a script
//! to run or a JSON or INI file to parse, all on the reference's terms: the native hands back.

use serde_json::{Map, Value};

use super::Host;

pub fn collect(host: &Host, fact_path: Option<&str>) -> Result<Map<String, Value>, String> {
    if let Some(dir) = fact_path
        && let Ok(entries) = std::fs::read_dir(host.root.path(dir))
    {
        // `glob('*.fact')`: a name starting with a dot is not matched.
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".fact") && !name.starts_with('.') {
                return Err(format!("{dir} holds local facts"));
            }
        }
    }
    let mut facts = Map::new();
    facts.insert("local".into(), Value::Object(Map::new()));
    Ok(facts)
}

#[cfg(test)]
mod tests;
