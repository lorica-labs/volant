// SPDX-License-Identifier: GPL-3.0-or-later
//! The arguments of `setup`, checked for what the native reproduces exactly.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::{COLLECTORS, DEFAULT_FACT_PATH, LEFT_OUT, MIN, RESTRICTED, SPEC};

/// What the task asks of `setup`, once the native knows it can answer it.
pub(super) struct Request {
    /// `gather_subset` as the reference converts it, which is also what its `gather_subset` fact
    /// says.
    pub(super) gather_subset: Vec<String>,
    /// Whether the reference runs its `hardware` collector.
    pub(super) hardware: bool,
    /// Whether the reference runs its `network` collector.
    pub(super) network: bool,
    /// `None` when the module would find no directory to read.
    pub(super) fact_path: Option<String>,
    /// `filter` as the reference converts it; empty keeps every fact.
    pub(super) filter: Vec<String>,
}

/// The arguments, checked for what the native reproduces exactly: a subset it collects, a filter
/// of strings, and a fact path the module would find empty.
pub(super) fn request(args: &Map<String, Value>) -> Result<Request, String> {
    if let Some(key) = args
        .keys()
        .find(|key| !SPEC.iter().any(|arg| arg.name == key.as_str()))
    {
        return Err(format!("argument '{key}' is outside the native setup"));
    }
    // `type='list', elements='str'`: a string is split on commas; a number would be converted
    // to text, which is left to the module.
    let filter: Vec<String> = match args.get("filter") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => text.split(',').map(str::to_string).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or("filter holds something other than strings")?,
        Some(_) => return Err("filter is not a list of strings".into()),
    };
    match args.get("gather_timeout") {
        None | Some(Value::Null) => {}
        Some(timeout) if timeout.is_i64() => {}
        Some(_) => return Err("gather_timeout is not an integer".into()),
    }
    let (gather_subset, collectors) = gather_subset(args.get("gather_subset"))?;
    let fact_path = match args.get("fact_path") {
        None => Some(DEFAULT_FACT_PATH.to_string()),
        Some(Value::Null) => None,
        // `type='path'` expands `~` and variables, and the local collector globs under it.
        Some(Value::String(path))
            if path.starts_with('/') && !path.contains(['~', '$', '*', '?', '[']) =>
        {
            Some(path.clone())
        }
        Some(_) => return Err("fact_path is not a plain absolute path".into()),
    };
    Ok(Request {
        gather_subset,
        hardware: collectors.contains("hardware"),
        network: collectors.contains("network"),
        fact_path,
        filter,
    })
}

/// `gather_subset` converted as `type='list', elements='str'` converts it, and the collectors
/// the reference resolves it to, if the native runs every one it names.
///
/// The resolution is the reference's `get_collector_names`: start from `min`, add what is named
/// (`all` adds every collector and fact id), remove what is negated (a collector with its fact
/// ids) unless it was also named, then add what the remaining collectors require. The native
/// answers when every `min` collector remains and every name given selects `min`, or `hardware`
/// or `network` for facts it produces; the other collectors `all` brings in are left out, their
/// facts absent. A name the reference does not know makes it fail, and is left to it.
pub(super) fn gather_subset(
    value: Option<&Value>,
) -> Result<(Vec<String>, BTreeSet<&'static str>), String> {
    let given: Vec<String> = match value {
        None => vec!["all".to_string()],
        Some(Value::String(text)) => text.split(',').map(str::to_string).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or("gather_subset holds something other than strings")?,
        Some(Value::Null) => return Err("gather_subset is null".into()),
        Some(_) => return Err("gather_subset is not a list".into()),
    };
    // `gather_subset or ['all']`: an empty list is everything, and the fact then says `['all']`.
    if given.is_empty() {
        return Err("an empty gather_subset is all".into());
    }
    let selecting = |name: &str| {
        COLLECTORS
            .iter()
            .filter(|(collector, ids, _)| *collector == name || ids.contains(&name))
            .map(|(collector, _, required)| (*collector, *required))
            .collect::<Vec<_>>()
    };
    let valid: BTreeSet<&str> = COLLECTORS
        .iter()
        .flat_map(|(collector, ids, _)| std::iter::once(*collector).chain(ids.iter().copied()))
        .collect();
    let mut added: BTreeSet<&str> = BTreeSet::new();
    let mut excluded: BTreeSet<&str> = BTreeSet::new();
    let mut named: BTreeSet<&str> = BTreeSet::new();
    for subset in std::iter::once("min").chain(given.iter().map(String::as_str)) {
        match subset.strip_prefix('!') {
            None if subset == "min" => added.extend(MIN),
            None if subset == "all" => added.extend(&valid),
            Some("min") => excluded.extend(MIN),
            Some("all") => excluded.extend(valid.iter().filter(|name| !MIN.contains(name))),
            Some(name) => {
                excluded.insert(name);
                for (collector, ids, _) in COLLECTORS {
                    if *collector == name {
                        excluded.extend(ids.iter().copied());
                    }
                }
            }
            None if valid.contains(subset) => {
                named.insert(subset);
                added.insert(subset);
            }
            None => {
                return Err("gather_subset names a subset the native does not know".into());
            }
        }
    }
    added.retain(|name| !excluded.contains(name) || named.contains(name));
    loop {
        let missing: Vec<&str> = added
            .iter()
            .flat_map(|name| selecting(name))
            .flat_map(|(_, required)| required.iter().copied())
            .filter(|required| !added.contains(required))
            .collect();
        if missing.is_empty() {
            break;
        }
        added.extend(missing);
    }
    let collectors: BTreeSet<&'static str> = added
        .iter()
        .flat_map(|name| selecting(name))
        .map(|(collector, _)| collector)
        .collect();
    if !MIN.iter().all(|name| collectors.contains(name)) {
        return Err("gather_subset leaves out part of min".into());
    }
    for name in named {
        let foreign = selecting(name)
            .iter()
            .any(|(collector, _)| !MIN.contains(collector) && !RESTRICTED.contains(collector));
        if foreign || LEFT_OUT.contains(&name) {
            return Err(format!(
                "gather_subset names '{name}', whose facts the native does not collect"
            ));
        }
    }
    Ok((given, collectors))
}
