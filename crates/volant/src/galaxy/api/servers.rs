// SPDX-License-Identifier: GPL-3.0-or-later
//! Several Galaxy servers asked in order (`MultiGalaxyAPIProxy`).

use super::Server;
use crate::galaxy::Failure;

/// Several servers, asked in order (`MultiGalaxyAPIProxy`).
pub struct Servers {
    pub list: Vec<Server>,
}

impl Servers {
    /// The versions of a collection on every server that answers, each with the index of its
    /// server. A server that cannot be reached is skipped with the reference's warning; an HTTP
    /// error skips it silently. When none answers, the last error is the result.
    pub fn versions(&mut self, fqcn: &str) -> Result<Vec<(String, usize)>, Failure> {
        let (namespace, name) = fqcn.split_once('.').unwrap_or((fqcn, ""));
        let mut found = Vec::new();
        let mut answered = false;
        let mut last = None;
        for (at, server) in self.list.iter_mut().enumerate() {
            match server.versions_of(namespace, name) {
                Ok(versions) => {
                    answered = true;
                    found.extend(versions.into_iter().map(|v| (v, at)));
                }
                Err(e) => {
                    if e.code == 0 {
                        eprintln!(
                            "[WARNING]: Skipping Galaxy server {}. Got an unexpected error when getting available versions of collection {fqcn}: {}",
                            server.api_server(),
                            e.failure.summary()
                        );
                    }
                    last = Some(e.failure);
                }
            }
        }
        match last {
            Some(failure) if !answered => Err(failure),
            _ => Ok(found),
        }
    }
}
