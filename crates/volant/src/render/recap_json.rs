// SPDX-License-Identifier: GPL-3.0-or-later
//! Ansible's JSON text, as the callback prints a result body.

/// Ansible dumps results as JSON with sorted keys and `": "` and `", "` separators.
pub(crate) fn ansible_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap_or_default(),
                        ansible_json(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(ansible_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// `json.dumps(value, indent=4, sort_keys=True, ensure_ascii=False)`: one key or item per line,
/// `,` at the end of each but the last, an empty mapping or list kept as `{}` or `[]`.
pub(crate) fn ansible_json_indented(value: &serde_json::Value, depth: usize) -> String {
    dumps_indented(value, depth, 4)
}

/// [`ansible_json_indented`] with `step` spaces per level.
pub(super) fn dumps_indented(value: &serde_json::Value, depth: usize, step: usize) -> String {
    let pad = |depth: usize| " ".repeat(step * depth);
    match value {
        serde_json::Value::Object(map) if !map.is_empty() => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}{}: {}",
                        pad(depth + 1),
                        serde_json::to_string(k).unwrap_or_default(),
                        dumps_indented(&map[k], depth + 1, step)
                    )
                })
                .collect();
            format!("{{\n{}\n{}}}", fields.join(",\n"), pad(depth))
        }
        serde_json::Value::Array(items) if !items.is_empty() => {
            let items: Vec<String> = items
                .iter()
                .map(|v| format!("{}{}", pad(depth + 1), dumps_indented(v, depth + 1, step)))
                .collect();
            format!("[\n{}\n{}]", items.join(",\n"), pad(depth))
        }
        other => ansible_json(other),
    }
}
