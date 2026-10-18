// SPDX-License-Identifier: GPL-3.0-or-later
//! The run's Python module union, kept on the controller between runs.
//!
//! Building the union costs a Python start, an ansible-core import and a cold build of every
//! module: 3.8 s of a 12.7 s converged run on the roles bench, measured. Checking that the files it
//! was built from are unchanged costs about 1 ms for the 154 of them. So a run first looks for an
//! entry under a key naming everything the build read that is not a file (this controller, the
//! interpreter, the modules asked for, the `ANSIBLE_*` and `PYTHON*` environment, the
//! `ansible.cfg` ansible-core would read, the working directory), then checks each file the helper
//! reported reading still has its size and mtime, and the zip still has its hash. Anything short
//! of that rebuilds, and nothing about the cache ever fails a run.
//!
//! An entry is only as trustworthy as the directory holding it, which is private to this user
//! (0700, owner checked): the same perimeter as `~/.ansible`, whose collections and modules a
//! build reads anyway.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::python::{Resolved, Union};

mod entry;
mod key;
mod sweep;

#[cfg(test)]
pub use entry::load;
pub use entry::{open, sources_from, store};
pub use key::located;
#[cfg(test)]
pub use key::{interpreter_without_running, key};

/// The manifest's own version. An entry written in another format is rebuilt, never read.
///
/// 2: each module's facts carry `core`. A format 1 entry has none, and read as `false` it would
/// keep every native module off for as long as the entry stays valid.
///
/// 3: an entry keeps the zip's deflated form beside it, `<key>.deflate`, named by its blake3 in
/// the manifest. A format 2 entry has none.
const FORMAT: u64 = 3;

/// An entry nothing has rewritten for this long is removed by the next store: a module set a
/// playbook no longer names would otherwise stay on disk for good.
const STALE: Duration = Duration::from_secs(30 * 24 * 3600);

/// Lowercase hex blake3 of everything an entry depends on that is not a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey(String);

/// One file a build read, as it was when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub len: u64,
    pub mtime_ns: i128,
}

impl Source {
    /// `path` as it is now, following a symbolic link as the helper's `os.stat` does.
    pub fn now(path: &Path) -> io::Result<Source> {
        let meta = fs::metadata(path)?;
        let mtime_ns = match meta.modified()?.duration_since(UNIX_EPOCH) {
            Ok(after) => i128::try_from(after.as_nanos()).unwrap_or(i128::MAX),
            Err(before) => -i128::try_from(before.duration().as_nanos()).unwrap_or(i128::MAX),
        };
        Ok(Source {
            path: path.to_path_buf(),
            len: meta.len(),
            mtime_ns,
        })
    }
}

/// What a run needs of a union without starting the helper: the union itself, the files it was
/// built from, and the answers the collections' names got, which the pre-flight checks the tasks
/// of this run against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub union: Union,
    pub sources: Vec<Source>,
    pub resolved: BTreeMap<String, Resolved>,
}

/// The interpreter a build would run under, found without running anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpreterId {
    /// The candidate as found: a virtualenv's `python` is a link to the system one, and the two
    /// import different ansible-cores.
    pub path: PathBuf,
    /// Where the links lead: a Nix profile's `python3` keeps its path and its wrapper's size
    /// across generations, and only the store path it resolves to moves.
    pub real: PathBuf,
    /// The size and mtime of the binary the link leads to, which an upgrade changes.
    pub len: u64,
    pub mtime_ns: i128,
}

/// Where a run looks for its union, and under which interpreter the entry must have been built:
/// the candidate `find_python` would start (`interpreter`), which has to be the executable the
/// helper reports running (`real`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub dir: PathBuf,
    pub key: CacheKey,
    pub interpreter: PathBuf,
    pub real: PathBuf,
}

/// What the helper says a union was built from: the interpreter that ran, as
/// `os.path.realpath(sys.executable)`, and every file it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traced {
    pub interpreter: PathBuf,
    pub sources: Vec<Source>,
}

#[cfg(test)]
mod tests;
