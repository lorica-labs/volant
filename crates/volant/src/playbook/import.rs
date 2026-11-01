// SPDX-License-Identifier: GPL-3.0-or-later
//! The keywords of an `import_playbook` entry: `when`, `tags` and `vars`, which reach every play
//! the file brings in.

use anyhow::{Context, bail};
use saphyr::{Scalar, Yaml};
use serde_json::Value;

use crate::yaml::{field, to_json};

use super::fields::{conditions, tags};
use super::model::Play;

/// Every attribute `PlaybookInclude` has in ansible-core 2.19.12, the name of the action apart.
/// A key outside this list refuses the load. Of these only `when`, `tags` and `vars` do
/// anything: measured, `ignore_errors`, `become`, `environment`, `no_log`, `run_once` and the rest
/// are accepted and not passed to the imported plays.
const ATTRIBUTES: &[&str] = &[
    "any_errors_fatal",
    "become",
    "become_exe",
    "become_flags",
    "become_method",
    "become_user",
    "check_mode",
    "connection",
    "debugger",
    "diff",
    "environment",
    "ignore_errors",
    "ignore_unreachable",
    "module_defaults",
    "name",
    "no_log",
    "port",
    "remote_user",
    "run_once",
    "tags",
    "throttle",
    "timeout",
    "vars",
    "when",
];

/// What an `import_playbook` entry says besides the file it names.
#[derive(Debug, Default)]
pub(super) struct Import {
    when: Vec<String>,
    tags: Vec<String>,
    /// The names of a `tags` among the variables of the entry, see [`Import::apply`].
    var_tags: Vec<String>,
    vars: crate::tags::Tagged,
}

impl Import {
    pub(super) fn read(entry: &Yaml) -> anyhow::Result<Self> {
        if let Some(map) = entry.as_mapping() {
            for (key, _) in map {
                let key = key.as_str().unwrap_or_default();
                if !key.ends_with("import_playbook") && !ATTRIBUTES.contains(&key) {
                    bail!("'{key}' is not a valid attribute for a PlaybookInclude");
                }
            }
        }
        let when = match field(entry, "when") {
            None | Some(Yaml::Value(Scalar::Null)) => Vec::new(),
            Some(_) => conditions(entry, "when", "")?,
        };
        refuse_template("when", &when)?;
        let mut vars = match field(entry, "vars") {
            None | Some(Yaml::Value(Scalar::Null)) => crate::tags::Tagged::default(),
            Some(node @ Yaml::Mapping(_)) => match to_json(node).context("'vars'")? {
                Value::Object(map) => super::origin::tagged_vars(node, map),
                _ => crate::tags::Tagged::default(),
            },
            Some(_) => bail!("Vars in a PlaybookInclude must be specified as a dictionary."),
        };
        let names = match field(entry, "tags") {
            None => Vec::new(),
            Some(node @ (Yaml::Value(Scalar::String(_)) | Yaml::Sequence(_))) => {
                if let Yaml::Sequence(items) = node {
                    for item in items {
                        refuse_item(item)?;
                    }
                }
                tags(entry, "")?
            }
            // Exit 1, not the 4 of a refused attribute, as the reference ends it.
            Some(_) => {
                return Err(crate::stats::Refusal::at(
                    1,
                    "tags must be specified as a list",
                ));
            }
        };
        refuse_template("tags", &names)?;
        let var_tags = match vars.remove("tags") {
            None => Vec::new(),
            Some(Value::String(text)) => text.split(',').map(str::to_string).collect(),
            Some(_) => {
                bail!("'tags' among the vars of an 'import_playbook' must be a string");
            }
        };
        refuse_template("tags", &var_tags)?;
        Ok(Import {
            when,
            tags: names,
            var_tags,
            vars,
        })
    }

    /// Puts what the entry says onto one play of the imported file.
    ///
    /// Measured on ansible-core 2.19.12: the tags join the play's, the variables of the entry
    /// win over the play's own, and the conditions go in front of those of every task, role and
    /// block of the play, handlers excepted. The play's fact gathering gets the entry's
    /// conditions in place of any set by an import further in.
    ///
    /// A `tags` among the variables of the entry leaves them, and its names join the play's tags
    /// only when the play wrote tags of its own: the reference extends the list the play holds,
    /// and a play that wrote none holds no list to extend.
    pub(super) fn apply(&self, play: &mut Play) {
        if !play.tags.is_empty() {
            play.tags.extend(self.var_tags.iter().cloned());
        }
        play.tags.extend(self.tags.iter().cloned());
        play.tags.sort_unstable();
        play.tags.dedup();
        play.vars.overlay(&self.vars);
        if !self.when.is_empty() {
            let inner = std::mem::take(&mut play.import_when);
            play.import_when = self.when.iter().cloned().chain(inner).collect();
            play.gather_when.clone_from(&self.when);
        }
    }
}

/// An entry of a `tags` list that is not text or a number, in the reference's words.
fn refuse_item(item: &Yaml) -> anyhow::Result<()> {
    let kind = match item {
        Yaml::Sequence(_) => "list",
        Yaml::Mapping(_) => "dict",
        _ => return Ok(()),
    };
    bail!(
        "Error processing keyword 'tags': Keyword 'tags' items must be of type 'str' or 'int', not '{kind}'."
    )
}

/// Refuses a keyword that holds a template. The reference renders `when` and `tags` of the entry
/// with the run's variables as it loads the playbook; this release has none at that point, and
/// reading the text as it stands would, for `tags`, select the wrong tasks without a word.
fn refuse_template(keyword: &str, values: &[String]) -> anyhow::Result<()> {
    if values
        .iter()
        .any(|v| v.contains("{{") || v.contains("{%") || v.contains("{#"))
    {
        bail!(
            "'{keyword}' of an 'import_playbook' holds a template, which this release does not read when it loads the playbook"
        );
    }
    Ok(())
}
