// SPDX-License-Identifier: GPL-3.0-or-later
//! Shapes more than one module's tests build.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::compile::{Compiled, TagSelection, compile};
use crate::playbook::{PlayTask, Playbook, parse};
use crate::roles::RoleSearch;
use crate::template::Templar;
use crate::transport::ConnectionDefaults;
use crate::vars::HostVars;

/// The entries of a JSON object as a playbook writes them: every string is trusted as a template.
pub(crate) fn written(value: &Value) -> crate::tags::Tagged {
    let origin = crate::tags::origin_described("test");
    let mut out = crate::tags::Tagged::default();
    for (name, value) in value.as_object().expect("an object") {
        let tree = crate::tags::uniform(value, origin, true);
        out.insert(name.clone(), value.clone(), tree);
    }
    out
}

/// A `hostvars` map whose hosts' variables are as a playbook writes them.
pub(crate) fn written_hostvars(hosts: crate::tags::Tagged) -> crate::vars::HostvarsMap {
    hosts
        .iter()
        .map(|(host, view)| {
            let entry = crate::vars::HostEntry::from(written(view));
            (host.clone(), std::sync::Arc::new(entry))
        })
        .collect()
}

/// The tags of `value` as a playbook writes it.
pub(crate) fn written_tree(value: &Value) -> Option<crate::tags::TagTree> {
    crate::tags::uniform(value, crate::tags::origin_described("test"), true)
}

/// Writes `value` under `name` as a playbook writes it.
pub(crate) fn put_written(map: &mut crate::tags::Tagged, name: &str, value: Value) {
    let tree = written_tree(&value);
    map.insert(name.to_string(), value, tree);
}

/// The names whose read by `host` finds what a managed host sent.
pub(crate) fn from_host_names(
    store: &mut crate::vars::VarStore,
    host: &str,
    scope: &crate::vars::Scope,
) -> std::collections::BTreeSet<String> {
    let layers = store.layered_for_host(host, scope);
    let held = layers.map.iter().map(|(name, _)| name);
    let facts = layers
        .facts
        .iter()
        .filter(|(_, v)| **v != Value::Object(Map::new()));
    held.filter(|name| layers.map.is_from_host(name))
        .chain(facts.map(|(name, _)| name))
        .chain(layers.data_lists.iter())
        .cloned()
        .collect()
}

/// The hosts `hostvars` holds a value of a managed host for, as `reader` sees them.
pub(crate) fn hosts_with_host_values(
    store: &mut crate::vars::VarStore,
    reader: &str,
) -> std::collections::BTreeSet<String> {
    let holds = |entry: &crate::vars::HostEntry| {
        let mut trees = entry.tags().values();
        trees.any(|tree| crate::tags::is_from_host(tree.own()))
    };
    let view = store.hostvars_shared(reader);
    let held = view.iter().filter(|(_, entry)| holds(entry));
    held.map(|(host, _)| host.clone()).collect()
}

/// Whether the name of `host` came from a managed host.
pub(crate) fn host_name_is_data(store: &mut crate::vars::VarStore, host: &str) -> bool {
    let scope = crate::vars::Scope::default();
    from_host_names(store, host, &scope).contains("inventory_hostname")
}

/// A directory removed when it goes out of scope, however the test ends.
pub(crate) fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("a scratch directory")
}

/// `text` written at `rel` under `dir`, parents created.
pub(crate) fn write(dir: &Path, rel: &str, text: &str) -> PathBuf {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the parent directory");
    std::fs::write(&path, text).expect("the fixture file");
    path
}

pub(crate) fn templar() -> Templar {
    Templar::new(std::env::temp_dir())
}

pub(crate) fn defaults() -> ConnectionDefaults {
    ConnectionDefaults {
        remote_user: None,
        connection: None,
        private_key: None,
        host_key_checking: true,
        remote_tmp: "~/.ansible/tmp".into(),
        connect_timeout: Duration::from_secs(10),
        r#become: false,
        become_user: "root".into(),
        become_method: "sudo".into(),
        become_exe: None,
        become_flags: None,
        become_password: None,
        ssh_common_args: Vec::new(),
        ssh_extra_args: Vec::new(),
        ssh_use_tty: true,
        names: crate::transport::ConnectionNames::default(),
        plugins: crate::transport::PluginSettings::default(),
    }
}

pub(crate) fn play(yaml: &str) -> Playbook {
    parse(yaml, "x.yml").unwrap_or_else(|e| panic!("{e:#}"))
}

/// The first play of `yaml`, compiled with no fact gathering, no roles and no tag selection.
pub(crate) fn compiled(yaml: &str) -> Compiled {
    let mut pb = play(yaml);
    pb.plays[0].gather_facts = false;
    compile(
        &pb.plays[0],
        &RoleSearch::default(),
        &TagSelection::default(),
    )
    .unwrap_or_else(|e| panic!("{e:#}"))
}

pub(crate) fn task(module: &str) -> PlayTask {
    PlayTask {
        name: "t".into(),
        module: module.into(),
        ..PlayTask::empty()
    }
}

/// A host's variables as the render path carries them: the map, and an empty shared view.
pub(crate) fn hvars(v: Value) -> HostVars {
    HostVars {
        map: crate::tags::Tagged::from(vars(v)),
        ..HostVars::default()
    }
}

pub(crate) fn vars(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}
