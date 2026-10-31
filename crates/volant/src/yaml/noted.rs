// SPDX-License-Identifier: GPL-3.0-or-later
//! Where values that a later failure points at were written: `!vault` values, and plain values
//! that hold a template.

use saphyr::{Scalar, Yaml};

use super::{Origins, is_vault};

/// Records, for every `!vault` value that is the value of a mapping entry of `docs`, where it is
/// written in `text`, the file `source`: a failure to read the value points to its tag, as
/// PyYAML marks it. A value inside a list has no position of its own and is left out.
///
/// A plain value that holds a template is recorded too, under its key, for a failure inside that
/// template to be placed at it.
pub(crate) fn note_value_origins(docs: &[Yaml<'_>], origins: &Origins, source: &str, text: &str) {
    let path: std::sync::Arc<str> = std::fs::canonicalize(source)
        .map_or_else(|_| source.to_string(), |p| p.display().to_string())
        .into();
    // The parser marks a block scalar where its content starts; the tag is the first `!vault` on
    // the key's line after the key.
    let tag_at = |(line, col): (usize, usize)| {
        let row = text.lines().nth(line - 1)?;
        let from = row.char_indices().nth(col - 1)?.0;
        let at = from + row[from..].find("!vault")?;
        Some((line, row[..at].chars().count() + 1))
    };
    let mut open: Vec<&Yaml<'_>> = docs.iter().collect();
    while let Some(node) = open.pop() {
        match node {
            Yaml::Sequence(items) => open.extend(items),
            Yaml::Mapping(map) => {
                for (key, value) in map {
                    if let (Some(key), Yaml::Value(Scalar::String(text))) = (key.as_str(), value)
                        && (text.contains("{{") || text.contains("{%"))
                        && let Some((line, col)) = origins.of_value(value)
                    {
                        let origin = crate::playbook::Origin {
                            path: path.clone(),
                            line,
                            col,
                        };
                        crate::template::note_written(key, text, origin);
                    }
                    if let Yaml::Tagged(tag, inner) = value
                        && tag.handle == "!"
                        && is_vault(&tag.suffix)
                        && let Yaml::Value(Scalar::String(text)) = &**inner
                        && let Some((line, col)) = origins
                            .of_key(value)
                            .and_then(tag_at)
                            .or_else(|| origins.of_value(value))
                    {
                        crate::vault::note_origin(
                            text,
                            crate::playbook::Origin {
                                path: path.clone(),
                                line,
                                col,
                            },
                        );
                    }
                    open.push(value);
                }
            }
            Yaml::Tagged(_, inner) => open.push(inner),
            _ => {}
        }
    }
}
