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
mod tests {
    use serde_json::json;

    use super::super::tests::{FakeRoot, probe};
    use super::*;

    /// What would make this red: a fact file answered as `{}`, which drops what the host
    /// declared; or a hidden file taken for one, which hands back a host the reference reads as
    /// empty.
    #[test]
    fn local_facts_are_empty_or_handed_back() {
        let fake = FakeRoot::new("local");
        let (root, probe) = (fake.root(), probe());
        let host = fake.host(&root, &probe);
        let dir = "/etc/ansible/facts.d";
        assert_eq!(
            collect(&host, Some(dir)).unwrap()["local"],
            json!({}),
            "no directory"
        );
        fake.write("/etc/ansible/facts.d/.hidden.fact", "{}")
            .write("/etc/ansible/facts.d/notes.txt", "");
        assert_eq!(collect(&host, Some(dir)).unwrap()["local"], json!({}));
        assert_eq!(collect(&host, None).unwrap()["local"], json!({}));
        fake.write("/etc/ansible/facts.d/site.fact", "{}");
        assert!(collect(&host, Some(dir)).is_err());
        assert!(collect(&host, None).is_ok(), "no fact path, nothing read");
    }
}
