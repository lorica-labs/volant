// SPDX-License-Identifier: GPL-3.0-or-later
//! The lookups that read the controller's own state: `inventory_hostnames` and `varnames`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use minijinja::value::ValueKind;
use minijinja::{Error, State, Value};

use super::{invalid, text_of};
use crate::inventory::Inventory;

/// `lookup('inventory_hostnames', pattern...)`: the hosts of the inventory the pattern selects,
/// matched with the pattern grammar `hosts:` and `--limit` use, over the `groups` the run has.
pub(super) fn inventory_hostnames(state: &State, terms: &[Value]) -> Result<Vec<Value>, Error> {
    let mut groups = BTreeMap::new();
    if let Some(all) = state.lookup("groups")
        && let Ok(names) = all.try_iter()
    {
        for name in names {
            let hosts = all.get_item(&name).ok().and_then(|h| h.try_iter().ok());
            let hosts = hosts.into_iter().flatten().map(|h| text_of(&h)).collect();
            groups.insert(text_of(&name), hosts);
        }
    }
    let pattern = terms.iter().map(text_of).collect::<Vec<_>>().join(":");
    let resolution = Inventory::from_groups(&groups).resolve(&pattern);
    // Said once per run, as the reference's display drops a warning it has already shown.
    static SHOWN: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    for unmatched in &resolution.unmatched {
        let line =
            format!("[WARNING]: Could not match supplied host pattern, ignoring: {unmatched}");
        if SHOWN.lock().expect("warnings shown").insert(line.clone()) {
            eprintln!("{line}");
        }
    }
    Ok(resolution
        .hosts
        .into_iter()
        .map(|h| Value::from(h.name))
        .collect())
}

/// `lookup('varnames', regex...)`: the names of the variables in scope that each regex finds
/// somewhere in them, term by term, as Python's `re.search` does.
pub(super) fn varnames(state: &State, terms: &[Value]) -> Result<Vec<Value>, Error> {
    let names: Vec<String> = state
        .lookup(crate::template::CONTEXT_KEY)
        .and_then(|root| root.try_iter().ok())
        .into_iter()
        .flatten()
        .map(|n| text_of(&n))
        .filter(|n| !n.starts_with("__volant_") && n != "volant_version")
        .collect();
    let mut found = Vec::new();
    for term in terms {
        let Some(pattern) = term.as_str() else {
            return Err(invalid(format!(
                "Invalid setting identifier, \"{term}\" is not a string, its a {}",
                python_type(term)
            )));
        };
        let regex = regex::Regex::new(pattern).map_err(|e| {
            invalid(format!(
                "Unable to use \"{pattern}\" as a search parameter: {e}"
            ))
        })?;
        found.extend(
            names
                .iter()
                .filter(|n| regex.is_match(n))
                .map(|n| Value::from(n.as_str())),
        );
    }
    Ok(found)
}

/// Python's name for the type of a template value, as an error message prints it.
pub(super) fn python_type(value: &Value) -> &'static str {
    match value.kind() {
        ValueKind::Bool => "<class 'bool'>",
        ValueKind::Number if value.is_integer() => "<class 'int'>",
        _ if crate::template::bigint::is_big(value) => "<class 'int'>",
        ValueKind::Number => "<class 'float'>",
        ValueKind::Seq => "<class 'list'>",
        ValueKind::Map => "<class 'dict'>",
        ValueKind::None => "<class 'NoneType'>",
        _ => "<class 'str'>",
    }
}
