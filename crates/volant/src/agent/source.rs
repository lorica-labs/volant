// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding the agent binaries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use super::embedded;

/// Where agent binaries live, looked up in this order: `VOLANT_AGENT_DIR`, the agents built into
/// this controller (extracted to a cache directory on first use), then the directory of this
/// executable. The local agent is `volant-agent`; cross-built ones are
/// `volant-agent-<target triple>`.
#[derive(Debug, Clone)]
pub struct AgentSource {
    places: Vec<Place>,
    read: Arc<Mutex<HashMap<PathBuf, Arc<AgentFile>>>>,
}

/// An agent binary as read off disk once, with its blake3 hash in hex: the bytes a link uploads
/// and the hash it compares a cached agent with are always the same bytes.
#[derive(Debug)]
pub struct AgentFile {
    pub bytes: Vec<u8>,
    pub hash: String,
}

#[derive(Debug, Clone)]
enum Place {
    Dir(PathBuf),
    Embedded(embedded::Embedded),
}

impl AgentSource {
    pub fn discover() -> Self {
        let exe_dir = std::env::current_exe().ok().and_then(beside_executable);
        Self::ordered(
            std::env::var_os("VOLANT_AGENT_DIR").map(PathBuf::from),
            embedded::Embedded::built_in(),
            exe_dir,
        )
    }

    /// The lookup order, apart from where each place comes from.
    pub(super) fn ordered(
        env_dir: Option<PathBuf>,
        embedded: Option<embedded::Embedded>,
        exe_dir: Option<PathBuf>,
    ) -> Self {
        let places = env_dir
            .map(Place::Dir)
            .into_iter()
            .chain(embedded.map(Place::Embedded))
            .chain(exe_dir.map(Place::Dir))
            .collect();
        Self {
            places,
            read: Arc::default(),
        }
    }

    /// The agent file at `path`, read and hashed on the first call and shared by every link of
    /// the run after that, clones of this source included. A file rebuilt during a run is not
    /// read again.
    pub fn read(&self, path: &Path) -> std::io::Result<Arc<AgentFile>> {
        if let Some(file) = self.lock().get(path) {
            return Ok(Arc::clone(file));
        }
        let bytes = std::fs::read(path)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        Ok(Arc::clone(
            self.lock()
                .entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(AgentFile { bytes, hash })),
        ))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Arc<AgentFile>>> {
        self.read.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn local(&self) -> anyhow::Result<PathBuf> {
        let file = format!("volant-agent{}", std::env::consts::EXE_SUFFIX);
        self.find(&file).map_err(|looked| {
            anyhow::anyhow!(
                "no executable agent binary '{file}' in {looked}. Set VOLANT_AGENT_DIR to the directory that holds it"
            )
        })
    }

    /// The agent to upload to a host of this target triple, or the places it was looked for in.
    pub fn for_target(&self, triple: &str) -> Result<PathBuf, String> {
        self.find(&format!("volant-agent-{triple}"))
    }

    /// The first place holding `file`, or every place looked in when none does, each with the
    /// reason it could not be used when there is one.
    pub(super) fn find(&self, file: &str) -> Result<PathBuf, String> {
        let mut looked = Vec::new();
        for place in &self.places {
            match place {
                Place::Dir(dir) => {
                    let path = dir.join(file);
                    if runnable(&path) {
                        return Ok(path);
                    }
                    looked.push(dir.display().to_string());
                }
                Place::Embedded(embedded) => match embedded.find(file) {
                    Ok(Some(path)) => return Ok(path),
                    Ok(None) => looked.push(embedded.describe()),
                    Err(err) => looked.push(format!("{}: {err}", embedded.describe())),
                },
            }
        }
        Err(looked.join(", "))
    }
}

/// The directory the agents ship in: the one holding the controller's own file, behind any link.
///
/// Linux answers `current_exe` with the resolved file, macOS with the path the program was
/// started through. An install that keeps the release directory whole and links `volant` onto
/// the `PATH`, which is what the install script does, would otherwise look for the agents in
/// the directory of the link.
pub(super) fn beside_executable(exe: PathBuf) -> Option<PathBuf> {
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent().map(Path::to_path_buf)
}

/// A file that carries an execute bit. A stray artifact with the right name is not an agent:
/// the local one would fail to spawn, and a cross-built one would be uploaded, made
/// executable on the host and then refuse to run there.
#[cfg(unix)]
pub(super) fn runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub(super) fn runnable(path: &Path) -> bool {
    path.is_file()
}
