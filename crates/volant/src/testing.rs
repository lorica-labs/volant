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
        map: vars(v),
        ..HostVars::default()
    }
}

pub(crate) fn vars(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}
