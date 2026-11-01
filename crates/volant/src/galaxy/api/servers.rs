// SPDX-License-Identifier: GPL-3.0-or-later
//! Several Galaxy servers asked in order (`MultiGalaxyAPIProxy`).

use std::collections::HashMap;

use super::Server;
use crate::galaxy::Failure;
use crate::galaxy::requirements::{Collection, Kind};
use crate::galaxy::settings::ServerSettings;

/// Several servers, asked in order (`MultiGalaxyAPIProxy`).
pub struct Servers {
    pub list: Vec<Server>,
    /// The collections a requirement's `source` sends to one server, by index in `list`.
    pub pinned: HashMap<String, usize>,
    /// How many servers at the front of `list` every other collection is asked on: the ones
    /// after them were added for a `source` alone.
    pub shared: usize,
}

impl Servers {
    /// Servers every collection is asked on.
    pub fn new(list: Vec<Server>) -> Self {
        let shared = list.len();
        Self {
            list,
            pinned: HashMap::new(),
            shared,
        }
    }

    /// The versions of a collection on every server that answers, each with the index of its
    /// server: the one its requirement's `source` names, else the shared ones. A server that
    /// cannot be reached is skipped with the reference's warning; an HTTP error skips it
    /// silently. When none answers, the last error is the result.
    pub fn versions(&mut self, fqcn: &str) -> Result<Vec<(String, usize)>, Failure> {
        let (namespace, name) = fqcn.split_once('.').unwrap_or((fqcn, ""));
        let asked = match self.pinned.get(fqcn) {
            Some(&at) => at..at + 1,
            None => 0..self.shared,
        };
        let mut found = Vec::new();
        let mut answered = false;
        let mut last = None;
        for (at, server) in self.list.iter_mut().enumerate() {
            if !asked.contains(&at) {
                continue;
            }
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

/// The server each Galaxy requirement of `wanted` with a `source` is installed from, by index in
/// `list`: a configured server whose name or URL the source is, else one added at the end of
/// `list` for that URL by `explicit` (`GalaxyCLI._init_coll_req_dict`).
///
/// # Errors
/// The reference's refusal of a `source` that is neither a configured server nor an HTTP URL.
pub fn pins(
    list: &mut Vec<ServerSettings>,
    wanted: &[Collection],
    explicit: impl Fn(&str, &str) -> ServerSettings,
) -> Result<HashMap<String, usize>, Failure> {
    let mut pinned = HashMap::new();
    for c in wanted.iter().filter(|c| c.kind == Kind::Galaxy) {
        let (Some(name), Some(source)) = (&c.name, &c.source) else {
            continue;
        };
        let at = match list
            .iter()
            .position(|s| s.name == *source || s.url == *source)
        {
            Some(at) => at,
            None if is_http_url(source) => {
                list.push(explicit(name, source));
                list.len() - 1
            }
            None => {
                return Err(Failure::error(format!(
                    "Collections requirement 'source' entry should contain a valid Galaxy API URL but it does not: {source} is not an HTTP URL."
                )));
            }
        };
        pinned.insert(name.clone(), at);
    }
    Ok(pinned)
}

/// A URL whose scheme is `http` or `https`, in any case (`_is_http_url`).
fn is_http_url(text: &str) -> bool {
    text.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}
