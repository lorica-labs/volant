// SPDX-License-Identifier: GPL-3.0-or-later
//! Which files of a repository are playbooks.

use std::path::{Path, PathBuf};

/// Directories that hold roles, variables, templates or tools, never a playbook to judge.
const SKIPPED: [&str; 13] = [
    "roles",
    "collections",
    "group_vars",
    "host_vars",
    "templates",
    "files",
    "defaults",
    "vars",
    "handlers",
    "meta",
    "molecule",
    "node_modules",
    "target",
];

/// The playbooks found, sorted, and how many YAML files were read and found not to be one.
#[derive(Debug, Default)]
pub(super) struct Found {
    pub playbooks: Vec<PathBuf>,
    pub skipped: usize,
}

/// The playbooks under `paths`. A directory is walked without following links and without
/// entering [`SKIPPED`] or hidden directories; a path given by name is followed even when it is a
/// link. More than `max_files` files walked is an error, so a check never wanders a whole disk.
pub(super) fn find(paths: &[PathBuf], max_files: usize) -> Result<Found, String> {
    let mut found = Found::default();
    let mut walked = 0;
    for path in paths {
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            walk(path, max_files, &mut walked, &mut found)?;
        } else {
            judge_file(path, &mut found);
        }
    }
    found.playbooks.sort();
    found.playbooks.dedup();
    Ok(found)
}

fn walk(dir: &Path, max: usize, walked: &mut usize, found: &mut Found) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            if !name.starts_with('.') && !SKIPPED.contains(&name.as_ref()) {
                walk(&entry.path(), max, walked, found)?;
            }
        } else if kind.is_file() {
            *walked += 1;
            if *walked > max {
                return Err(format!(
                    "more than {max} files under {}: give --max-files a larger number, or name the directories that hold the playbooks",
                    dir.display()
                ));
            }
            if name.ends_with(".yml") || name.ends_with(".yaml") {
                judge_file(&entry.path(), found);
            }
        }
    }
    Ok(())
}

fn judge_file(path: &Path, found: &mut Found) {
    if is_playbook(path) {
        found.playbooks.push(path.to_path_buf());
    } else {
        found.skipped += 1;
    }
}

/// A list with an element that has `hosts` or `import_playbook`. A file that is not YAML, a
/// vaulted one included, is not a playbook.
fn is_playbook(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(docs) = crate::yaml::load_plain(&text, &path.to_string_lossy()) else {
        return false;
    };
    let Some(items) = docs.first().and_then(|doc| doc.as_sequence()) else {
        return false;
    };
    items.iter().any(|item| {
        [
            "hosts",
            "import_playbook",
            "ansible.builtin.import_playbook",
            "ansible.legacy.import_playbook",
        ]
        .iter()
        .any(|key| crate::yaml::field(item, key).is_some())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playbooks_are_found_and_the_rest_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("site.yml", "- hosts: all\n  tasks: []\n");
        write("all.yaml", "- import_playbook: site.yml\n");
        write(
            "playbooks/db.yml",
            "- ansible.builtin.import_playbook: ../site.yml\n",
        );
        write("vars.yml", "key: value\n");
        write("tasks.yml", "- debug: msg=hi\n");
        write("broken.yml", "- hosts: [\n");
        write("roles/r/tasks/main.yml", "- hosts: all\n");
        write("group_vars/all.yml", "- hosts: all\n");
        write(".github/workflows/ci.yml", "- hosts: all\n");
        write("README.md", "- hosts: all\n");
        let found = find(&[root.to_path_buf()], 100).unwrap();
        let names: Vec<_> = found
            .playbooks
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(names, ["all.yaml", "playbooks/db.yml", "site.yml"]);
        assert_eq!(found.skipped, 3);
        let err = find(&[root.to_path_buf()], 2).unwrap_err();
        assert!(err.contains("more than 2 files"), "{err}");
    }
}
