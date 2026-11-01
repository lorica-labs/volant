// SPDX-License-Identifier: GPL-3.0-or-later
//! The module an `action:` or `local_action:` statement names, and its arguments.

use anyhow::bail;
use saphyr::{Scalar, Yaml};
use serde_json::Value;

use super::keywords::{possibly_all_template, warn_action_mapping};

/// Where the module of an `action:` string ends: at the first space outside a template, as
/// ansible-core's `split_args` joins a template's pieces back together. Measured on 2.19.12,
/// `action: "{{ prefix }}file path=x"` runs `file` when `prefix` is empty.
fn module_word_end(text: &str) -> usize {
    let mut depth = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match (c, chars.peek().map(|&(_, next)| next)) {
            ('{', Some('{' | '%')) => {
                depth += 1;
                chars.next();
            }
            ('}' | '%', Some('}')) if depth > 0 => {
                depth -= 1;
                chars.next();
            }
            (c, _) if depth == 0 && c.is_whitespace() => return at,
            _ => {}
        }
    }
    text.len()
}

/// The module an `action:` or `local_action:` value names and the value that module reads its
/// arguments from, as if it had been written under the module's own key.
///
/// Measured on ansible-core 2.19.12: `action: command hostname` and `action: {module: command,
/// cmd: hostname}` both run, the mapping with a deprecation warning. In the mapping, `module:`
/// may carry `key=value` words after the name, which win over the keys beside it, and an `args:`
/// mapping wins over both (`_normalize_old_style_args`). A first word with a `=` in it is a
/// `module=command` spelling the reference cannot resolve, which fails the task rather than the
/// load, and a mapping with no `module:` has "no module/action".
pub(super) fn action_statement<'a>(node: &Yaml<'a>) -> anyhow::Result<(String, Yaml<'a>)> {
    match node {
        Yaml::Value(Scalar::String(text)) => {
            let text = text.trim();
            let end = module_word_end(text);
            let (name, rest) = (&text[..end], text[end..].trim_start());
            if name.is_empty() {
                bail!("no module/action detected in task.");
            }
            let args = if rest.is_empty() {
                Yaml::Value(Scalar::Null)
            } else {
                Yaml::Value(Scalar::String(rest.to_string().into()))
            };
            Ok((name.to_string(), args))
        }
        Yaml::Mapping(entries) => {
            warn_action_mapping(node);
            let (mut name, mut words) = (None, String::new());
            let mut args = saphyr::Mapping::new();
            let mut inner = saphyr::Mapping::new();
            for (key, value) in entries {
                match (key.as_str(), value) {
                    (Some("module"), Yaml::Value(Scalar::String(module))) => {
                        let module = module.trim();
                        let end = module_word_end(module);
                        name = Some(module[..end].to_string());
                        words = module[end..].trim_start().to_string();
                    }
                    (Some("args"), Yaml::Mapping(given)) => inner.extend(given.clone()),
                    // A template is an argument splat, which the task reads for itself.
                    (Some("args"), Yaml::Value(Scalar::String(text)))
                        if possibly_all_template(text) => {}
                    _ => {
                        args.insert(key.clone(), value.clone());
                    }
                }
            }
            let Some(name) = name.filter(|name| !name.is_empty()) else {
                bail!("no module/action detected in task.");
            };
            if !words.is_empty() {
                let said = Yaml::Value(Scalar::String(words.into()));
                for (key, value) in super::fields::module_args(&name, &said)? {
                    if let Value::String(value) = value {
                        args.insert(
                            Yaml::Value(Scalar::String(key.into())),
                            Yaml::Value(Scalar::String(value.into())),
                        );
                    }
                }
            }
            args.extend(inner);
            Ok((name, Yaml::Mapping(args)))
        }
        _ => bail!("no module/action detected in task."),
    }
}
