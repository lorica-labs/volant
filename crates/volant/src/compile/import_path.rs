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
        let templar = Templar::new(self.file_dir.clone());
        match templar.render(name, &self.compile_vars()) {
            Ok(Value::String(path)) => Ok(path),
            Ok(other) => Ok(other.to_string()),
            Err(err) => bail!(
                "Error when evaluating variable in import path '{name}': {}",
                err.0
            ),
        }
    }

    /// A step's tags, a template among them rendered against the same variables as an import's
    /// path: measured on ansible-core 2.19.12, `tags: "{{ the_tags }}"` over a play's `vars:`
    /// lists and selects as the tags it renders to. One that cannot be rendered stays as written.
    pub(super) fn rendered_tags(&self, tags: &[String]) -> Vec<String> {
        if !tags.iter().any(|t| Templar::is_template(t)) {
            return tags.to_vec();
        }
        let vars = self.compile_vars();
        let templar = Templar::new(self.file_dir.clone());
        let mut out = Vec::new();
        for tag in tags {
            if !Templar::is_template(tag) {
                out.push(tag.clone());
                continue;
            }
            match templar.render(tag, &vars) {
                Ok(Value::Array(items)) => out.extend(items.iter().map(|i| match i {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })),
                Ok(Value::String(text)) => out.extend(
                    text.split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(str::to_string),
                ),
                Ok(other) => out.push(other.to_string()),
                Err(_) => out.push(tag.clone()),
            }
        }
        out
    }

    /// What a template renders against while the play compiles, before any host: the roles'
    /// `defaults`, the play's `vars:`, the extra variables and `playbook_dir`, the roles' `vars`,
    /// and `role_path`.
    fn compile_vars(&self) -> Map<String, Value> {
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
        vars
    }
}
