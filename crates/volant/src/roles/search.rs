// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a role is found on disk.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::stats::Refusal;

use super::{folded, identity};

/// Where a role is looked for.
///
/// Measured on ansible-core 2.19.12: with no `roles_path` set, a missing role names
/// `<playbook_dir>/roles`, `~/.ansible/roles`, `/usr/share/ansible/roles`, `/etc/ansible/roles`
/// and `<playbook_dir>`, in that order and separated by colons. A `roles_path` from `ansible.cfg`
/// or from `ANSIBLE_ROLES_PATH` replaces the three middle entries rather than adding to them,
/// and the environment wins over the file - both measured the same way, by reading the list a
/// missing role prints.
#[derive(Debug, Clone, Default)]
pub struct RoleSearch {
    pub paths: Vec<PathBuf>,
    pub collections: Vec<PathBuf>,
}

impl RoleSearch {
    /// The playbook's directory, whose `roles/` is the first place searched.
    pub fn playbook_dir(&self) -> Option<&Path> {
        self.paths.first().and_then(|roles| roles.parent())
    }

    pub fn new(playbook_dir: &Path, config: &Config) -> Self {
        // The reference makes every one of these absolute and folds `.` and `..` out of the text
        // (`unfrackpath`); the search path a role's lookups read is built from them, and so is
        // what an `included:` line shows.
        let playbook_dir = folded(playbook_dir);
        let mut paths = vec![playbook_dir.join("roles")];
        paths.extend(config.roles_path.iter().cloned());
        paths.push(playbook_dir.clone());
        let mut collections = vec![playbook_dir.join("collections")];
        collections.extend(config.collections_path.iter().cloned());
        Self { paths, collections }
    }

    /// The directory a role name points at.
    ///
    /// A role found on the search path is named by its real path, links followed, as the
    /// reference's `unfrackpath` names it: a `~/.ansible/roles/geerlingguy.nginx` link to a
    /// checkout makes `included:` lines and `role_path` show the checkout.
    ///
    /// A three-part name is a collection role and is looked for under
    /// `<collection path>/ansible_collections/<namespace>/<collection>/roles/<role>`; measured,
    /// a `collections/` directory beside the playbook is searched, and the paths a missing one
    /// names are the ordinary ones and not the collection ones.
    pub fn locate(&self, name: &str) -> anyhow::Result<PathBuf> {
        self.find(name, None, &[])
    }

    /// A role a `meta/main.yml` names, `owner` being the role that holds the file.
    ///
    /// ansible-core 2.19.12 also searches the directory that holds the owner, after `roles_path`
    /// and before the playbook's directory (`RoleDefinition._role_basedir`). kubespray's
    /// `roles/kubernetes-apps/policy_controller` depends on `policy_controller/calico`, found as
    /// `roles/kubernetes-apps/policy_controller/calico`. A missing one names the collections the
    /// owner searches in front of the paths, `ansible.legacy` last among them, measured.
    pub fn locate_dependency(
        &self,
        name: &str,
        owner: &Path,
        collections: &[String],
    ) -> anyhow::Result<PathBuf> {
        let mut searched = Vec::new();
        if let (_, Some(own)) = identity(owner, "") {
            searched.push(own);
        }
        for collection in collections.iter().chain([&"ansible.legacy".to_string()]) {
            if !searched.contains(collection) {
                searched.push(collection.clone());
            }
        }
        self.find(name, owner.parent(), &searched)
    }

    fn find(
        &self,
        name: &str,
        basedir: Option<&Path>,
        collections: &[String],
    ) -> anyhow::Result<PathBuf> {
        // The playbook's directory is the last entry, and the owner's goes in front of it.
        let mut paths: Vec<&Path> = self.paths.iter().map(PathBuf::as_path).collect();
        if let Some(basedir) = basedir {
            paths.insert(paths.len().saturating_sub(1), basedir);
        }
        for base in &paths {
            let candidate = base.join(name);
            if candidate.is_dir() {
                return Ok(std::fs::canonicalize(&candidate).unwrap_or(candidate));
            }
        }
        let parts: Vec<&str> = name.split('.').collect();
        if let [namespace, collection, role] = parts[..] {
            for base in &self.collections {
                let candidate = base
                    .join("ansible_collections")
                    .join(namespace)
                    .join(collection)
                    .join("roles")
                    .join(role);
                if candidate.is_dir() {
                    return Ok(candidate);
                }
            }
        }
        // Exit 1, measured, and notably not the 4 the rest of a playbook this engine cannot make
        // sense of gets. It is built with its own code so the blanket code the compiler wraps its
        // failures in cannot bury it.
        Err(Refusal::at(
            1,
            format!(
                "the role '{name}' was not found in {}",
                collections
                    .iter()
                    .cloned()
                    .chain(paths.iter().map(|p| p.display().to_string()))
                    .collect::<Vec<_>>()
                    .join(":")
            ),
        ))
    }
}
