// SPDX-License-Identifier: GPL-3.0-or-later
//! The path of an import, rendered while the play compiles.

use anyhow::bail;
use serde_json::{Map, Value};

use crate::template::Templar;

use super::builder::Builder;
use super::inherit::extend;

impl Builder<'_> {
    /// An import's path, rendered when it holds a template. Measured on ansible-core 2.19.12: it
    /// reads `role_path`, `playbook_dir`, the play's `vars:`, `-e` and the roles' `defaults` and
    /// `vars`, and an inventory variable is undefined there, which stops the run at exit 4.
    pub(super) fn import_path(&self, name: &str) -> anyhow::Result<String> {
        if !Templar::is_template(name) {
            return Ok(name.to_string());
        }
        let mut vars = Map::new();
        for role in &self.roles {
            extend(&mut vars, &role.defaults);
        }
        extend(&mut vars, &self.static_vars);
        for role in &self.roles {
            extend(&mut vars, &role.vars);
        }
        if let Some(dir) = &self.role_dir {
            vars.insert("role_path".into(), Value::String(dir.display().to_string()));
        }
        let templar = Templar::new(self.file_dir.clone());
        match templar.render(name, &vars) {
            Ok(Value::String(path)) => Ok(path),
            Ok(other) => Ok(other.to_string()),
            Err(err) => bail!(
                "Error when evaluating variable in import path '{name}': {}",
                err.0
            ),
        }
    }
}
