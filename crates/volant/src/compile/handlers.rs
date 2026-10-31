// SPDX-License-Identifier: GPL-3.0-or-later
//! A handlers list read into the play, with the files its `import_tasks` statements name.

use std::path::Path;

use serde_json::Value;
use volant_protocol::modules::short_name;

use crate::playbook::{HandlerTask, PlayTask};
use crate::stats::Refusal;

use super::builder::{Builder, DEPTH, RECURSION};
use super::inherit::merge;
use super::model::Handler;

impl Builder<'_> {
    /// A handlers list, each handler with the keywords of what holds it, and every `import_tasks`
    /// in it replaced by the handlers of the file it names, read beside the file that wrote it.
    /// Measured on ansible-core 2.19.12: a handler such a file defines is found by name and by
    /// `listen`, a role's under the role's name.
    pub(super) fn push_handlers(
        &mut self,
        handlers: &[HandlerTask],
        kw: &PlayTask,
        dir: &Path,
        role: Option<usize>,
    ) -> anyhow::Result<()> {
        for handler in handlers {
            let task = merge(kw, &handler.task);
            if short_name(&handler.task.module) != "import_tasks" {
                let mut origin = (*self.origin()).clone();
                origin.file_dir = dir.to_path_buf();
                self.handlers.push(Handler {
                    task,
                    listen: handler.listen.clone(),
                    role,
                    origin: std::sync::Arc::new(origin),
                });
                continue;
            }
            let name = handler
                .task
                .args
                .get("file")
                .or_else(|| handler.task.args.get("_raw_params"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("'import_tasks' takes a file name"))?;
            // Beside the file that wrote it, then in the role's directory, then in the playbook's:
            // measured, a role's `handlers/main.yml` importing `tasks/handlers.yml` reads the
            // role's, and importing `handlers.yml` reads the playbook's when the role has none.
            let role_dir = role.and(self.origin().role_dir.clone());
            let path = [Some(dir), role_dir.as_deref(), self.search.playbook_dir()]
                .into_iter()
                .flatten()
                .map(|dir| dir.join(name))
                .find(|path| path.is_file())
                .unwrap_or_else(|| dir.join(name));
            if !path.is_file() {
                return Err(Refusal::at(
                    1,
                    format!(
                        "Unable to retrieve file contents.
Could not find or access '{}' on the Ansible Controller.",
                        path.display()
                    ),
                ));
            }
            self.depth += 1;
            if self.depth > DEPTH {
                return Err(Refusal::at(1, RECURSION));
            }
            let imported = crate::playbook::parse_handlers_file(&path)?;
            let result = self.push_handlers(
                &imported,
                &task,
                path.parent().unwrap_or(Path::new(".")),
                role,
            );
            self.depth -= 1;
            result?;
        }
        Ok(())
    }
}
