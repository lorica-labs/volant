// SPDX-License-Identifier: GPL-3.0-or-later
//! `-e` as `ansible-inventory` reads it: each inventory plugin reads it before its source, so one
//! that cannot be read fails every plugin, then ends the run.

use std::path::Path;

use serde_json::Value;

/// Why `-e` cannot be read, worded as the reference words it: the message, its `Origin:` when it
/// has one, and the exit code the run ends with.
pub(super) struct Unreadable {
    pub message: String,
    pub origin: Option<String>,
    pub code: i32,
    /// Whether the reference ends the `[ERROR]:` block with an empty line, as for a missing file.
    pub spaced: bool,
}

impl Unreadable {
    /// The `[ERROR]:` text that ends the run.
    pub fn text(&self) -> String {
        match &self.origin {
            Some(origin) => format!("{}\nOrigin: {origin}\n", self.message),
            None if self.spaced => format!("{}\n", self.message),
            None => self.message.clone(),
        }
    }
}

/// The first `-e` item the reference cannot read, measured on ansible-core 2.19.12: invalid YAML
/// exits 4, a missing file 1, and a value that is no mapping or a path without `@` exit 5.
pub(super) fn unreadable(items: &[String], base: &Path) -> Option<Unreadable> {
    let refused = |message: String| Unreadable {
        message,
        origin: None,
        code: 5,
        spaced: false,
    };
    for item in items.iter().filter(|item| !item.is_empty()) {
        let value = if let Some(file) = item.strip_prefix('@') {
            let path = base.join(file);
            let Ok(text) = std::fs::read_to_string(&path) else {
                let p = path.display();
                return Some(Unreadable {
                    message: format!(
                        "Unable to retrieve file contents.\nCould not find or access '{p}' on the Ansible Controller.\nIf you are using a module and expect the file to exist on the remote, see the remote_src option: [Errno 2] No such file or directory: '{p}'"
                    ),
                    origin: None,
                    code: 1,
                    spaced: true,
                });
            };
            loaded(&text, &path.display().to_string())?
        } else if item.starts_with(['/', '.']) {
            return Some(refused(format!(
                "Please prepend extra_vars filename '{item}' with '@'"
            )));
        } else if item.starts_with(['[', '{']) {
            loaded(item, "<CLI option '-e'>")?
        } else {
            continue;
        };
        match value {
            Ok(Value::Object(_)) => {}
            Ok(_) => {
                return Some(refused(format!(
                    "Invalid extra vars data supplied. '{item}' could not be made into a dictionary"
                )));
            }
            Err(unreadable) => return Some(unreadable),
        }
    }
    None
}

/// The first document of `text`, or the reference's syntax error for it. `None` for an error
/// [`crate::vars::parse_extra_vars`] reports as it always has.
fn loaded(text: &str, source: &str) -> Option<Result<Value, Unreadable>> {
    let docs = match crate::yaml::load(text, source) {
        Ok(docs) => docs,
        Err(err) => {
            let (message, line, col) = crate::yaml::syntax_error(&err, text)?;
            return Some(Err(Unreadable {
                message,
                origin: Some(format!(
                    "{source}:{line}:{col}\n\n(source not shown: TypeError)"
                )),
                code: 4,
                spaced: false,
            }));
        }
    };
    let value = docs.first().map(crate::yaml::to_json).transpose().ok()?;
    Some(Ok(value.unwrap_or(Value::Null)))
}
