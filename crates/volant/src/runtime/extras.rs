// SPDX-License-Identifier: GPL-3.0-or-later
//! The optional wheel sets of `manifest.toml` (`windows`, `network`), installed into a runtime
//! that is already in place, and the run's refusal of a connection whose set it lacks.

use std::fs;
use std::path::Path;

use serde_json::{Map, Value};

use super::host_triple;
use super::install::{
    Installed, Lock, RECORD, WAIT, install_wheels, lock_path, partial_prefix, python_command,
};
use super::manifest::{EXTRAS, Manifest, Wheel, imports};
use crate::inventory::Host;
use crate::transport::ConnectionDefaults;

/// Refuses a name `--extras` gives that is no set.
pub(crate) fn known(sets: &[String]) -> Result<(), String> {
    match sets
        .iter()
        .find(|set| !EXTRAS.iter().any(|(name, _)| name == set))
    {
        Some(bad) => Err(format!(
            "there is no runtime extras set named {bad:?}: the sets are {}",
            EXTRAS.map(|(name, _)| name).join(", ")
        )),
        None => Ok(()),
    }
}

/// The sets the runtime in `dir` holds, as its record lists them.
pub(crate) fn installed_sets(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join(RECORD))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|record| Some(record.get("extras")?.as_object()?.keys().cloned().collect()))
        .unwrap_or_default()
}

/// Installs the sets of `sets` that `runtime` lacks, each from its pinned wheels with no
/// dependency resolution, so the base wheels stay as they are. A set is recorded once it
/// imports, so a set that failed halfway is installed again next time.
pub(crate) fn install(
    runtimes: &Path,
    manifest: &Manifest,
    runtime: &Installed,
    sets: &[String],
) -> Result<(), String> {
    known(sets)?;
    let missing = || {
        let have = installed_sets(&runtime.dir);
        sets.iter()
            .filter(|set| !have.contains(set))
            .cloned()
            .collect::<Vec<_>>()
    };
    if missing().is_empty() {
        return Ok(());
    }
    let triple = host_triple()?;
    let platform = manifest.platform(&triple)?;
    let _lock = Lock::acquire(&lock_path(runtimes, &runtime.id), WAIT)?;
    for set in missing() {
        let wheels = platform
            .extras
            .get(&set)
            .ok_or_else(|| format!("the manifest pins no {set:?} extras for {triple}"))?;
        eprintln!(
            "Installing the {set:?} runtime extras into {} ...",
            runtime.id
        );
        let work = tempfile::Builder::new()
            .prefix(&partial_prefix(&runtime.id))
            .tempdir_in(runtimes)
            .map_err(|e| {
                format!(
                    "cannot create a directory in {}: {e}; check the free disk space",
                    runtimes.display()
                )
            })?;
        let what = format!("the {set:?} extras");
        install_wheels(&runtime.python, wheels, work.path(), &what, &["--no-deps"])?;
        probe(&runtime.python, &set)?;
        record(&runtime.dir, &set, wheels)?;
    }
    Ok(())
}

/// Checks that the runtime imports what the set is for.
fn probe(python: &Path, set: &str) -> Result<(), String> {
    let output = python_command(python)
        .args(["-c", &format!("import {}", imports(set))])
        .output()
        .map_err(|e| format!("cannot run {}: {e}", python.display()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "the runtime does not import {} after the {set:?} extras were installed: {}",
        imports(set),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Adds the set to the runtime's record, written beside it and renamed over it.
fn record(dir: &Path, set: &str, wheels: &[Wheel]) -> Result<(), String> {
    let path = dir.join(RECORD);
    let text =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut record: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    let listed: Vec<Value> = wheels
        .iter()
        .map(|wheel| {
            serde_json::json!({
                "name": wheel.name,
                "version": wheel.version,
                "sha256": wheel.file.sha256,
            })
        })
        .collect();
    let fields = record
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))?;
    fields
        .entry("extras")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| format!("the extras of {} are not a JSON object", path.display()))?
        .insert(set.to_owned(), Value::Array(listed));
    let partial = dir.join(format!("{RECORD}.partial"));
    fs::write(&partial, format!("{record:#}\n"))
        .and_then(|()| fs::rename(&partial, &path))
        .map_err(|e| {
            format!(
                "cannot write {}: {e}; check the free disk space",
                path.display()
            )
        })
}

/// The set a connection plugin needs, by the names ansible-core knows it under.
pub(crate) fn set_for(connection: &str) -> Option<&'static str> {
    match connection {
        "winrm" | "psrp" | "ansible.builtin.winrm" | "ansible.builtin.psrp" => Some("windows"),
        "network_cli"
        | "netconf"
        | "ansible.netcommon.network_cli"
        | "ansible.netcommon.netconf" => Some("network"),
        _ => None,
    }
}

/// The refusal for the first host whose connection needs a set `have` lacks.
pub(crate) fn refusal<'a>(
    connections: impl IntoIterator<Item = (&'a str, String)>,
    have: &[String],
) -> Option<String> {
    connections.into_iter().find_map(|(host, connection)| {
        let set = set_for(&connection).filter(|set| !have.iter().any(|had| had == set))?;
        Some(format!(
            "host '{host}': the {connection} connection needs the \"{set}\" runtime extras: run \
             volant runtime install --extras {set}"
        ))
    })
}

/// Refuses a play's hosts before anything connects when one's connection needs a set the
/// controller's runtime lacks. Only Volant's own runtime is checked: a user's interpreter is
/// theirs, and the plugin's own error names the packages it needs. The connection is read as
/// the run starts (inventory variables, extra variables, the play's keyword, `-c`), so one a
/// `set_fact` gives later fails at its task instead.
pub(crate) fn check(
    hosts: &[Host],
    play_connection: Option<&str>,
    extra: &Map<String, Value>,
    defaults: &ConnectionDefaults,
) -> Result<(), String> {
    let connection = |host: &Host| {
        [extra, &host.vars]
            .iter()
            .find_map(|vars| crate::vars::host_setting(*vars, "ansible_connection"))
            .and_then(Value::as_str)
            .or(play_connection)
            .or(defaults.connection.as_deref())
            .unwrap_or("ssh")
            .to_owned()
    };
    if !hosts
        .iter()
        .any(|host| set_for(&connection(host)).is_some())
    {
        return Ok(());
    }
    let (Some(python), Ok(runtimes), Ok(manifest)) = (
        super::resolve::controller_python(),
        super::runtimes_dir(),
        super::resolve::pinned(),
    ) else {
        return Ok(());
    };
    let dir = runtimes.join(manifest.id());
    if !Path::new(&python).starts_with(&dir) {
        return Ok(());
    }
    let have = installed_sets(&dir);
    refusal(
        hosts
            .iter()
            .map(|host| (host.name.as_str(), connection(host))),
        &have,
    )
    .map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::scratch;

    #[test]
    fn a_connection_whose_set_is_missing_is_refused_with_the_command() {
        let hosts = [
            ("lin", "ssh".to_owned()),
            ("win", "ansible.builtin.winrm".to_owned()),
        ];
        assert_eq!(
            refusal(hosts.clone(), &[]).as_deref(),
            Some(
                "host 'win': the ansible.builtin.winrm connection needs the \"windows\" runtime \
                 extras: run volant runtime install --extras windows"
            )
        );
        assert_eq!(refusal(hosts, &["windows".to_owned()]), None);
        let net = [("sw", "network_cli".to_owned())];
        assert!(
            refusal(net, &["windows".to_owned()])
                .unwrap()
                .contains("--extras network")
        );
    }

    #[test]
    fn a_set_is_recorded_beside_the_base_wheels() {
        let dir = scratch();
        fs::write(dir.path().join(RECORD), r#"{"id":"r","wheels":[]}"#).unwrap();
        assert!(installed_sets(dir.path()).is_empty());
        let wheel = Wheel {
            name: "pywinrm".into(),
            version: "0.5.0".into(),
            file: super::super::manifest::Artifact {
                url: "https://example.org/pywinrm-0.5.0-py3-none-any.whl".into(),
                sha256: "ab".repeat(32),
            },
        };
        record(dir.path(), "windows", &[wheel]).unwrap();
        record(dir.path(), "network", &[]).unwrap();
        assert_eq!(installed_sets(dir.path()), ["windows", "network"]);
        let text = fs::read_to_string(dir.path().join(RECORD)).unwrap();
        let record: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(record["id"], "r");
        assert_eq!(record["extras"]["windows"][0]["version"], "0.5.0");
    }

    #[test]
    fn an_unknown_set_is_refused_with_the_known_ones() {
        assert!(known(&["windows".into(), "network".into()]).is_ok());
        let e = known(&["linux".into()]).unwrap_err();
        assert!(
            e.contains("\"linux\"") && e.contains("windows, network"),
            "{e}"
        );
    }
}
