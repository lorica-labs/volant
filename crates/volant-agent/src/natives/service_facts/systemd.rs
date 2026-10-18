// SPDX-License-Identifier: GPL-3.0-or-later
//! `systemctl`'s units and unit files, and the `show` for each unit `list-units` did not name.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};

use super::imp::{Listed, Services, command, plain, py_space};
use crate::natives::runtime::{Clock, Stop, py_split};
/// The states `SystemctlScanService` reads anywhere on a line but the last word.
const BAD_STATES: [&str; 3] = ["not-found", "masked", "failed"];

/// How many `systemctl show` run at once.
const SHOWS_AT_ONCE: usize = 8;

/// `SystemctlScanService`: `list-units`, then `list-unit-files`, then a `show` for each unit
/// file the first did not list. The `show`s run beside each other and are read in the
/// reference's order.
pub(super) fn systemd<'a>(
    systemctl: &Path,
    env: &BTreeMap<String, String>,
    clock: impl Fn() -> Clock<'a> + Copy + Send,
) -> Listed {
    let (rc, units) = command(
        env,
        clock(),
        systemctl,
        &[
            "list-units",
            "--no-pager",
            "--type",
            "service",
            "--all",
            "--plain",
        ],
    )?;
    if rc != 0 {
        return Err(
            format!("systemctl list-units exited {rc}, which the module warns about").into(),
        );
    }
    let mut services = from_units(&units)?;
    let (rc, files) = command(
        env,
        clock(),
        systemctl,
        &[
            "list-unit-files",
            "--no-pager",
            "--type",
            "service",
            "--all",
        ],
    )?;
    if rc != 0 {
        return Err(
            format!("systemctl list-unit-files exited {rc}, which the module warns about").into(),
        );
    }
    let files = unit_files(&files)?;
    let unlisted = unlisted(&services, &files);
    let states = show_all(systemctl, env, clock, &unlisted)?;
    merge_unit_files(&mut services, &files, &states);
    Ok(services)
}

/// `_list_from_units`: one entry per line that mentions `.service`.
pub(super) fn from_units(stdout: &str) -> Result<Services, String> {
    let mut services: Services = Vec::new();
    for line in stdout.split('\n').filter(|line| line.contains(".service")) {
        let fields: Vec<&str> = py_split(line).collect();
        if fields.len() < 4 {
            return Err("a unit line has fewer than four fields".into());
        }
        let head = &fields[..fields.len() - 1];
        let bad: Vec<&str> = BAD_STATES
            .iter()
            .copied()
            .filter(|bad| head.contains(bad))
            .collect();
        // The reference tries its states in a frozenset's order, which changes with each
        // run's hash seed: with two on one line, its answer is a coin toss.
        let status = match bad[..] {
            [] => fields[2],
            [one] => one,
            _ => return Err("a unit line names two failure states".into()),
        };
        let state = if fields[3] == "running" {
            "running"
        } else {
            "stopped"
        };
        let mut service = Map::new();
        service.insert("name".into(), Value::from(fields[0]));
        service.insert("state".into(), Value::from(state));
        service.insert("status".into(), Value::from(status));
        service.insert("source".into(), Value::from("systemd"));
        insert(&mut services, fields[0], service);
    }
    Ok(services)
}

/// A dict's assignment: an existing key keeps its place and takes the new value.
pub(super) fn insert(
    services: &mut Vec<(String, Map<String, Value>)>,
    name: &str,
    service: Map<String, Value>,
) {
    match services.iter_mut().find(|(known, _)| known == name) {
        Some(slot) => slot.1 = service,
        None => services.push((name.to_string(), service)),
    }
}

/// `list-unit-files`' lines that mention `.service`, as name and state. A line with fewer
/// than two words makes the reference raise.
pub(super) fn unit_files(stdout: &str) -> Result<Vec<(String, String)>, String> {
    stdout
        .split('\n')
        .filter(|line| line.contains(".service"))
        .map(|line| {
            let mut words = py_split(line);
            match (words.next(), words.next()) {
                (Some(name), Some(state)) => Ok((name.to_string(), state.to_string())),
                _ => Err("a unit file line has fewer than two words".into()),
            }
        })
        .collect()
}

/// The unit files the reference runs `show` for: those not yet in its dict when their line
/// comes, each once.
pub(super) fn unlisted(
    services: &[(String, Map<String, Value>)],
    files: &[(String, String)],
) -> Vec<String> {
    let mut unlisted: Vec<String> = Vec::new();
    for (name, _) in files {
        if !services.iter().any(|(known, _)| known == name) && !unlisted.contains(name) {
            unlisted.push(name.clone());
        }
    }
    unlisted
}

/// `_list_from_unit_files`' updates, given each unlisted unit's `show` answer.
pub(super) fn merge_unit_files(
    services: &mut Vec<(String, Map<String, Value>)>,
    files: &[(String, String)],
    states: &BTreeMap<String, (i32, String)>,
) {
    for (name, status) in files {
        match services.iter_mut().find(|(known, _)| known == name) {
            None => {
                let (rc, stdout) = &states[name];
                let state = if *rc == 0 && !stdout.is_empty() {
                    stdout
                        .replace("ActiveState=", "")
                        .trim_end_matches(py_space)
                        .to_string()
                } else {
                    "unknown".to_string()
                };
                let mut service = Map::new();
                service.insert("name".into(), Value::from(name.as_str()));
                service.insert("state".into(), Value::from(state));
                service.insert("status".into(), Value::from(status.as_str()));
                service.insert("source".into(), Value::from("systemd"));
                services.push((name.clone(), service));
            }
            Some((_, service)) => {
                let current = service["status"].as_str().unwrap_or_default();
                if !BAD_STATES.contains(&current) {
                    service.insert("status".into(), Value::from(status.as_str()));
                }
            }
        }
    }
}

/// `systemctl show <unit> --property=ActiveState` for every unit, at most `SHOWS_AT_ONCE`
/// at a time. A name a shell would change goes through `/bin/sh -c`, as the reference sends
/// every one of them.
pub(super) fn show_all<'a>(
    systemctl: &Path,
    env: &BTreeMap<String, String>,
    clock: impl Fn() -> Clock<'a> + Copy + Send,
    units: &[String],
) -> Result<BTreeMap<String, (i32, String)>, Stop> {
    let chunk = units.len().div_ceil(SHOWS_AT_ONCE).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = units
            .chunks(chunk)
            .map(|units| {
                scope.spawn(move || {
                    units
                        .iter()
                        .map(|unit| Ok((unit.clone(), show(systemctl, env, clock(), unit)?)))
                        .collect::<Result<Vec<_>, Stop>>()
                })
            })
            .collect();
        let mut states = BTreeMap::new();
        for worker in workers {
            states.extend(worker.join().expect("a show thread panicked")?);
        }
        Ok(states)
    })
}

pub(super) fn show(
    systemctl: &Path,
    env: &BTreeMap<String, String>,
    clock: Clock,
    unit: &str,
) -> Result<(i32, String), Stop> {
    if plain(unit, "") {
        return command(
            env,
            clock,
            systemctl,
            &["show", unit, "--property=ActiveState"],
        );
    }
    let line = format!("{} show {unit} --property=ActiveState", systemctl.display());
    command(env, clock, Path::new("/bin/sh"), &["-c", &line])
}
