// SPDX-License-Identifier: GPL-3.0-or-later
//! Ansible Vault: the 1.1 and 1.2 formats of ansible-core 2.19.12, read and written, and the
//! secrets a run is given for them. A secret lives in memory only: it is never logged, never
//! passed to a child process and never sent to an agent.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, RwLock};

mod format;
mod load;

pub use format::{decrypt, encrypt};
pub use load::{Request, load};

const HEADER: &[u8] = b"$ANSIBLE_VAULT";

/// What went wrong with vault data, in the reference's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultError(pub String);

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VaultError {}

/// The passwords of a run. Every one is tried on every piece of data, whatever its label: the
/// reference does not match labels strictly (measured).
#[derive(Default, Clone)]
pub struct Secrets(Vec<Vec<u8>>);

impl Secrets {
    pub fn new(passwords: Vec<Vec<u8>>) -> Self {
        Self(passwords)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The first secret, the one the `vault` filter and `encrypt` use when none is named.
    #[cfg(test)]
    pub fn first(&self) -> Option<&[u8]> {
        self.0.first().map(Vec::as_slice)
    }
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secrets({} redacted)", self.0.len())
    }
}

static RUN_SECRETS: RwLock<Option<Arc<Secrets>>> = RwLock::new(None);

/// Makes the secrets of this run readable by the templating and file loading. A later call
/// replaces the earlier set.
pub fn install(secrets: Secrets) {
    *RUN_SECRETS.write().expect("secrets lock") = Some(Arc::new(secrets));
    REVEALED.lock().expect("revealed lock").clear();
    NAMES.lock().expect("names lock").clear();
}

type Revealed = HashMap<String, Result<String, String>>;

/// What each `!vault` value of the run decrypted to, so a value read by every task of every host
/// pays for its key derivation once. Held in memory only, like the secrets.
static REVEALED: LazyLock<Mutex<Revealed>> = LazyLock::new(Mutex::default);

/// The text of one vaulted value under the run's secrets, or the reference's reason it cannot be
/// read.
pub fn reveal(ciphertext: &str) -> Result<String, String> {
    if let Some(done) = REVEALED.lock().expect("revealed lock").get(ciphertext) {
        return done.clone();
    }
    let done = decrypt(ciphertext.as_bytes(), &run_secrets())
        .map_err(|e| e.0)
        .and_then(|plain| {
            String::from_utf8(plain).map_err(|_| "the decrypted value is not valid UTF-8".into())
        });
    REVEALED
        .lock()
        .expect("revealed lock")
        .insert(ciphertext.to_string(), done.clone());
    done
}

/// The secrets of this run; empty when none were given.
pub fn run_secrets() -> Arc<Secrets> {
    RUN_SECRETS
        .read()
        .expect("secrets lock")
        .clone()
        .unwrap_or_default()
}

/// The reason as a sentence of the reference's: its `AnsibleError`s end in a period, the format
/// errors it raises from the cipher do not.
pub fn sentence(reason: &str) -> String {
    if reason.starts_with("Vault format") || reason.starts_with("Vault vaulttext format") {
        reason.to_string()
    } else {
        format!("{}.", reason.trim_end_matches('.'))
    }
}

/// The names of the variables that held a `!vault` value, for `NAME is vault_encrypted`.
/// Kept by name for the whole run, not per host.
static NAMES: LazyLock<Mutex<std::collections::BTreeSet<String>>> = LazyLock::new(Mutex::default);

pub fn note_name(name: &str) {
    NAMES.lock().expect("names lock").insert(name.to_string());
}

pub fn is_name(name: &str) -> bool {
    NAMES.lock().expect("names lock").contains(name)
}

/// A text file, decrypted first when it is vaulted: every file the reference reads through its
/// loader.
pub fn read_to_string(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let bytes = if is_encrypted(&bytes) {
        decrypt(&bytes, &run_secrets()).map_err(|e| std::io::Error::other(sentence(&e.0)))?
    } else {
        bytes
    };
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )
    })
}

/// Whether `data` starts like vault data.
pub fn is_encrypted(data: &[u8]) -> bool {
    data.starts_with(HEADER)
}

#[cfg(test)]
mod tests;
