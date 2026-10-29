// SPDX-License-Identifier: GPL-3.0-or-later
//! Inventory variable names ansible-core 2.19.12 still accepts and deprecates.

use std::path::Path;

use saphyr::Yaml;

use super::ParseCx;
use crate::playbook::Origin;

/// Whether `name` may name a variable: an identifier, and none of the constants Jinja reads
/// before any variable. Measured: `none`, `true`, `False`, `1abc`, `a-b`, `a.b` and `x y` are
/// deprecated, `def` and `class` are not.
fn valid(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
        && !matches!(name, "true" | "false" | "none" | "True" | "False" | "None")
}

/// Deprecates `name` at `origin` when it is no variable name, once per name and place.
fn note(cx: &ParseCx, name: &str, origin: Origin) {
    if valid(name) {
        return;
    }
    cx.deprecate_once(format!(
        "Accepting inventory variable with invalid name '{name}'. This feature will be removed \
         from ansible-core version 2.23.\n{}\n\nVariable names must be strings starting with a \
         letter or underscore character, and contain only letters, numbers and underscores.",
        crate::render::source_context(&origin)
    ));
}

/// The names written on line `line` of an INI file, placed at the whole line as the reference
/// places them.
pub(super) fn note_line<'a>(
    cx: &ParseCx,
    file: &Path,
    line: usize,
    names: impl IntoIterator<Item = &'a String>,
) {
    let path = std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf());
    for name in names {
        let path = path.display().to_string().into();
        note(cx, name, Origin { path, line, col: 0 });
    }
}

/// Whether a YAML inventory names a variable the reference deprecates, read before the file is
/// parsed again for where each one is written.
pub(super) fn any_invalid(root: &Yaml<'_>) -> bool {
    let mut found = false;
    names(root, &mut |name, _| found |= !valid(name));
    found
}

/// The names of a YAML inventory, in the order they are written, each at its key.
pub(super) fn note_yaml(
    cx: &ParseCx,
    root: &Yaml<'_>,
    origins: &crate::yaml::Origins,
    file: &Path,
) {
    let path = std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf());
    names(root, &mut |name, value| {
        if let Some((line, col)) = origins.of_key(value) {
            let path = path.display().to_string().into();
            note(cx, name, Origin { path, line, col });
        }
    });
}

/// Every variable name of the groups in `root`, with the value it names: a group's `vars`, its
/// hosts' own and its children's, as written.
fn names<'a>(root: &'a Yaml<'a>, each: &mut impl FnMut(&str, &'a Yaml<'a>)) {
    fn entries<'a>(node: &'a Yaml<'a>) -> impl Iterator<Item = (&'a Yaml<'a>, &'a Yaml<'a>)> {
        node.as_mapping().into_iter().flatten()
    }
    fn group<'a>(node: &'a Yaml<'a>, each: &mut impl FnMut(&str, &'a Yaml<'a>)) {
        for (key, value) in entries(node) {
            match key.as_str() {
                Some("vars") => {
                    for (name, value) in entries(value) {
                        if let Some(name) = name.as_str() {
                            each(name, value);
                        }
                    }
                }
                Some("hosts") => {
                    for (_, host) in entries(value) {
                        for (name, value) in entries(host) {
                            if let Some(name) = name.as_str() {
                                each(name, value);
                            }
                        }
                    }
                }
                Some("children") => {
                    for (_, child) in entries(value) {
                        group(child, each);
                    }
                }
                _ => {}
            }
        }
    }
    for (_, node) in entries(root) {
        group(node, each);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_name_is_an_identifier_and_no_jinja_constant() {
        for name in ["a", "_x", "ok_name", "def", "class", "é"] {
            assert!(super::valid(name), "{name}");
        }
        for name in [
            "", "none", "true", "False", "None", "1abc", "a-b", "a.b", "x y",
        ] {
            assert!(!super::valid(name), "{name}");
        }
    }
}
