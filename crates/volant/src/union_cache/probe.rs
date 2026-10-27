// SPDX-License-Identifier: GPL-3.0-or-later
//! A candidate interpreter's `import ansible`, remembered between runs.
//!
//! Before it uses an interpreter, a run checks that it imports ansible-core: a Python start and
//! an import, 0.25 to 0.45 s on a loaded machine, paid again by every run. A yes is kept under
//! the interpreter (the candidate's path, the binary it leads to, its size and mtime) and the
//! environment that decides what it imports (`PYTHON*`, `HOME`), with the stamp of the
//! `ansible/__init__.py` it imported. A run takes it back only while that file is still there,
//! unchanged; anything else asks the interpreter again. A no is never kept. The entries live in
//! the union cache's directory, under the same checks.

use std::ffi::OsString;
use std::path::Path;

use serde_json::{Value, json};

use super::entry::read_own;
use super::sweep::{check_dir, write_new};
use super::{InterpreterId, Source};
use crate::agent::embedded::create_private;

/// Whether `dir` holds a yes for `python` that still stands.
pub fn recalled(dir: &Path, python: &str) -> bool {
    let Some(name) = name(python) else {
        return false;
    };
    if check_dir(dir).is_err() {
        return false;
    }
    let Some(kept) = read_own(&dir.join(name))
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return false;
    };
    let (Some(path), Some(len), Some(mtime_ns)) = (
        kept["ansible"].as_str(),
        kept["len"].as_u64(),
        kept["mtime_ns"].as_i64(),
    ) else {
        return false;
    };
    Source::now(Path::new(path))
        .is_ok_and(|now| now.len == len && now.mtime_ns == i128::from(mtime_ns))
}

/// Keeps the yes `python` just gave, `ansible` being the `ansible/__init__.py` it imported.
/// Nothing about the cache ever fails a run: a yes that cannot be kept is asked for again.
pub fn remember(dir: &Path, python: &str, ansible: &Path) {
    let (Some(name), Ok(found), Some(path)) =
        (name(python), Source::now(ansible), ansible.to_str())
    else {
        return;
    };
    let Ok(mtime_ns) = i64::try_from(found.mtime_ns) else {
        return;
    };
    if create_private(dir).and_then(|()| check_dir(dir)).is_err() {
        return;
    }
    let kept = json!({"ansible": path, "len": found.len, "mtime_ns": mtime_ns});
    let _ = write_new(dir, &name, kept.to_string().as_bytes());
}

/// `<key>.probe`, or `None` for an interpreter no key can name (a script, a missing file).
fn name(python: &str) -> Option<String> {
    let id = super::key::interpreter_id(python)?;
    let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    Some(format!("{}.probe", key(&id, &env)))
}

/// Lowercase hex blake3 of the interpreter and of the `PYTHON*` and `HOME` variables in `env`.
fn key(id: &InterpreterId, env: &[(OsString, OsString)]) -> String {
    let mut hasher = blake3::Hasher::new();
    let mut field = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(b"probe");
    field(id.path.as_os_str().as_encoded_bytes());
    field(id.real.as_os_str().as_encoded_bytes());
    field(&id.len.to_le_bytes());
    field(&id.mtime_ns.to_le_bytes());
    let mut read: Vec<&(OsString, OsString)> = env
        .iter()
        .filter(|(name, _)| {
            let name = name.as_encoded_bytes();
            name.starts_with(b"PYTHON") || name == b"HOME"
        })
        .collect();
    read.sort();
    for (name, value) in read {
        field(name.as_encoded_bytes());
        field(value.as_encoded_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> InterpreterId {
        InterpreterId {
            path: "/venv/bin/python".into(),
            real: "/usr/bin/python3.13".into(),
            len: 1,
            mtime_ns: 2,
        }
    }

    #[test]
    fn the_key_moves_with_the_interpreter_and_what_it_imports_by() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(OsString, OsString)> {
            pairs
                .iter()
                .map(|(n, v)| ((*n).into(), (*v).into()))
                .collect()
        };
        let base = key(&id(), &env(&[("HOME", "/h")]));
        assert_eq!(base, key(&id(), &env(&[("HOME", "/h"), ("TERM", "x")])));
        assert_ne!(
            base,
            key(&id(), &env(&[("HOME", "/h"), ("PYTHONPATH", "/p")]))
        );
        assert_ne!(base, key(&id(), &env(&[("HOME", "/other")])));
        let upgraded = InterpreterId {
            mtime_ns: 3,
            ..id()
        };
        assert_ne!(base, key(&upgraded, &env(&[("HOME", "/h")])));
        let other_venv = InterpreterId {
            path: "/other/bin/python".into(),
            ..id()
        };
        assert_ne!(base, key(&other_venv, &env(&[("HOME", "/h")])));
    }

    #[cfg(unix)]
    #[test]
    fn a_yes_stands_while_the_ansible_it_imported_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("volant").join("unions");
        std::fs::create_dir_all(tmp.path().join("volant")).unwrap();
        let python = tmp.path().join("python");
        std::fs::write(&python, b"\x7fELF").unwrap();
        let python = python.to_str().unwrap();
        let ansible = tmp.path().join("__init__.py");
        std::fs::write(&ansible, "").unwrap();
        assert!(!recalled(&dir, python));
        remember(&dir, python, &ansible);
        assert!(recalled(&dir, python));
        std::fs::write(&ansible, "changed").unwrap();
        assert!(!recalled(&dir, python));
        remember(&dir, python, &ansible);
        std::fs::remove_file(&ansible).unwrap();
        assert!(!recalled(&dir, python));
    }
}
