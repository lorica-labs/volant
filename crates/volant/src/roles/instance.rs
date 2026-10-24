// SPDX-License-Identifier: GPL-3.0-or-later
//! One compiled role instance: its variables and the names it goes by.

use std::path::Path;

use serde_json::{Map, Value};

/// One compiled role instance: the variables its own steps sit on, and the ones it lends to the
/// rest of the play.
#[derive(Debug, Clone, Default)]
pub struct RoleVars {
    pub name: String,
    /// The role's own name and collection, as ansible-core's `Role` holds them (see
    /// [`identity`]): what `role_name`, `ansible_role_name` and `ansible_collection_name` read.
    pub role_name: String,
    pub collection: Option<String>,
    pub defaults: Map<String, Value>,
    pub vars: Map<String, Value>,
    pub params: Map<String, Value>,
    /// Whether this instance lends `defaults` and `vars` to the rest of the play.
    pub public: bool,
    /// What the role itself says, set aside when the rest of the play's values were first laid
    /// under it, so that a variable another role publishes later can be laid under it again.
    pub own: Option<Box<OwnVars>>,
}

impl RoleVars {
    /// `ansible_role_name`: the name with its collection in front, `Role.get_name()`.
    pub fn full_name(&self) -> String {
        match &self.collection {
            Some(collection) => format!("{collection}.{}", self.role_name),
            None => self.role_name.clone(),
        }
    }
}

/// A role's own name and its collection, from the directory it was found in and the name it was
/// asked for by, as ansible-core's `RoleDefinition` works them out: a role found under
/// `ansible_collections/<namespace>/<collection>/roles/<role>` is `<role>` of
/// `<namespace>.<collection>`, one named by a path is the last part of that path, and any other
/// is the name as written (`geerlingguy.postgresql`).
pub fn identity(path: &Path, asked: &str) -> (String, Option<String>) {
    let parts: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if let [
        ..,
        "ansible_collections",
        namespace,
        collection,
        "roles",
        role,
    ] = parts[..]
    {
        return (role.to_string(), Some(format!("{namespace}.{collection}")));
    }
    let name = if asked.contains('/') {
        Path::new(asked)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(asked)
    } else {
        asked
    };
    (name.to_string(), None)
}

/// The `defaults` and `vars` a role directory holds, before anything was laid under them.
#[derive(Debug, Clone, Default)]
pub struct OwnVars {
    pub defaults: Map<String, Value>,
    pub vars: Map<String, Value>,
    /// What the other roles of the same statement lend: its dependencies, which see each other
    /// and the role that asked for them whether or not the statement is public.
    pub group_defaults: Map<String, Value>,
    pub group_vars: Map<String, Value>,
}
