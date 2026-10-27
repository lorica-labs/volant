// SPDX-License-Identifier: GPL-3.0-or-later
//! The dependency resolver of `collection install`: one version per collection, each meeting
//! every range asked of it, the highest first, backtracking on a conflict. ansible-galaxy 2.19.12
//! runs `resolvelib` over `CollectionDependencyProvider` (`dependency_resolution/providers.py`,
//! `reporters.py`); this follows its choices on the cases measured, not its internals.

use std::collections::HashMap;

use super::Failure;
use super::version::{Version, is_pin, is_pre_release, meets};

/// What the resolver asks of the servers or of the tarballs at hand.
pub trait Index {
    /// Every version of a collection, in any order.
    fn versions(&mut self, fqcn: &str) -> Result<Vec<String>, Failure>;
    /// What a version depends on: `namespace.name` and a range.
    fn dependencies(&mut self, fqcn: &str, version: &str)
    -> Result<Vec<(String, String)>, Failure>;
}

/// A range asked of a collection, by the command line or by a pinned collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub fqcn: String,
    pub range: String,
    /// The collection and version that asked, `None` for a direct request.
    pub parent: Option<(String, String)>,
}

impl Request {
    pub fn direct(fqcn: &str, range: &str) -> Self {
        Self {
            fqcn: fqcn.into(),
            range: range.into(),
            parent: None,
        }
    }

    fn line(&self) -> String {
        let from = match &self.parent {
            None => "direct request".to_string(),
            Some((fqcn, version)) => format!("dependency of {fqcn}:{version}"),
        };
        format!("* {}:{} ({from})", self.fqcn, self.range)
    }
}

/// What changes the candidates.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// `--pre`.
    pub pre: bool,
    /// `--upgrade`.
    pub upgrade: bool,
    /// `--no-deps`.
    pub no_deps: bool,
    /// Installed collections, `namespace.name` and version, taken first when they fit.
    pub preferred: Vec<(String, String)>,
}

/// The pins, in the order the collections were first asked for.
pub type Resolution = Vec<(String, String)>;

#[derive(Clone, Default)]
struct State {
    order: Vec<String>,
    criteria: HashMap<String, Vec<Request>>,
    pins: HashMap<String, String>,
}

impl State {
    fn ask(&mut self, request: Request) {
        if !self.criteria.contains_key(&request.fqcn) {
            self.order.push(request.fqcn.clone());
        }
        self.criteria
            .entry(request.fqcn.clone())
            .or_default()
            .push(request);
    }
}

struct Resolver<'a> {
    index: &'a mut dyn Index,
    options: &'a Options,
    rejections: HashMap<String, usize>,
    versions: HashMap<String, Vec<String>>,
    /// The requests of the last collection that could not be met.
    conflict: Vec<Request>,
}

/// Resolves `requests`, printing what ansible-galaxy prints while it backtracks.
pub fn resolve(
    requests: &[Request],
    index: &mut dyn Index,
    options: &Options,
) -> Result<Resolution, Failure> {
    let mut state = State::default();
    for request in requests {
        state.ask(request.clone());
    }
    let mut resolver = Resolver {
        index,
        options,
        rejections: HashMap::new(),
        versions: HashMap::new(),
        conflict: Vec::new(),
    };
    for fqcn in state.order.clone() {
        if resolver.matches(&fqcn, &state.criteria[&fqcn])?.is_empty() {
            return Err(impossible(&state.criteria[&fqcn]));
        }
    }
    match resolver.solve(state)? {
        Some(done) => Ok(done
            .order
            .iter()
            .map(|fqcn| (fqcn.clone(), done.pins[fqcn].clone()))
            .collect()),
        None => Err(impossible(&resolver.conflict)),
    }
}

impl Resolver<'_> {
    fn solve(&mut self, state: State) -> Result<Option<State>, Failure> {
        let Some(fqcn) = state
            .order
            .iter()
            .find(|f| !state.pins.contains_key(*f))
            .cloned()
        else {
            return Ok(Some(state));
        };
        for version in self.matches(&fqcn, &state.criteria[&fqcn])? {
            let mut next = state.clone();
            next.pins.insert(fqcn.clone(), version.clone());
            let dependencies = if self.options.no_deps {
                Vec::new()
            } else {
                self.index.dependencies(&fqcn, &version)?
            };
            let mut conflicted = None;
            for (dependency, range) in dependencies {
                next.ask(Request {
                    fqcn: dependency.clone(),
                    range,
                    parent: Some((fqcn.clone(), version.clone())),
                });
                let asked = next.criteria[&dependency].clone();
                let met = match next.pins.get(&dependency) {
                    Some(pinned) => asked.iter().all(|r| meets(pinned, &r.range)),
                    None => !self.matches(&dependency, &asked)?.is_empty(),
                };
                if !met {
                    conflicted = Some(asked);
                    break;
                }
            }
            if let Some(asked) = conflicted {
                self.conflict = asked;
                self.reject(&fqcn);
                continue;
            }
            if let Some(done) = self.solve(next)? {
                return Ok(Some(done));
            }
        }
        Ok(None)
    }

    /// The versions of `fqcn` that meet every request, highest first: an installed one alone
    /// when it fits and `--upgrade` is not given, as the reference takes it without asking a
    /// server.
    fn matches(&mut self, fqcn: &str, asked: &[Request]) -> Result<Vec<String>, Failure> {
        let fits = |version: &str| asked.iter().all(|r| meets(version, &r.range));
        let installed: Vec<String> = self
            .options
            .preferred
            .iter()
            .filter(|(f, v)| f == fqcn && fits(v))
            .map(|(_, v)| v.clone())
            .collect();
        if !installed.is_empty() && !self.options.upgrade {
            return Ok(installed);
        }
        if !self.versions.contains_key(fqcn) {
            let versions = self.index.versions(fqcn)?;
            self.versions.insert(fqcn.to_string(), versions);
        }
        let all = &self.versions[fqcn];
        let releases_exist = all.iter().any(|v| !is_pre_release(v));
        let pinned: Vec<&str> = asked
            .iter()
            .filter(|r| is_pin(&r.range))
            .map(|r| r.range.trim_start_matches('=').trim())
            .collect();
        let mut found: Vec<&String> = all
            .iter()
            .filter(|v| fits(v))
            .filter(|v| {
                !(is_pre_release(v)
                    && releases_exist
                    && !self.options.pre
                    && !pinned.contains(&v.as_str()))
            })
            .collect();
        found.sort_by(|a, b| {
            Version::parse(b)
                .zip(Version::parse(a))
                .map_or(std::cmp::Ordering::Equal, |(b, a)| b.cmp(&a))
        });
        // Under `--upgrade`, an installed version still comes first when nothing newer matches.
        let newest = found.first().and_then(|v| Version::parse(v));
        let mut out: Vec<String> = installed
            .into_iter()
            .filter(|v| {
                newest
                    .as_ref()
                    .is_none_or(|n| Version::parse(v).is_some_and(|v| &v >= n))
            })
            .collect();
        out.extend(found.into_iter().cloned());
        Ok(out)
    }

    /// Counts a rejected candidate and prints the reference's line at its counts.
    fn reject(&mut self, fqcn: &str) {
        let count = self.rejections.entry(fqcn.to_string()).or_default();
        *count += 1;
        match *count {
            1 | 8 => println!(
                "ansible-galaxy is looking at multiple versions of {fqcn} to determine which version is compatible with other requirements. This could take a while."
            ),
            13 => println!(
                "This is taking longer than usual. You might need to provide the dependency resolver with stricter constraints to reduce runtime. If you want to abort this run, press Ctrl + C."
            ),
            _ => {}
        }
    }
}

/// The reference's error when no resolution exists. Its message ends with a dump of Python
/// objects after the hint, which is not reproduced.
fn impossible(asked: &[Request]) -> Failure {
    let lines: Vec<String> = asked.iter().map(Request::line).collect();
    Failure::error(format!(
        "Failed to resolve the requested dependencies map. Could not satisfy the following requirements:\n{}\nHint: Pre-releases hosted on Galaxy or Automation Hub are not installed by default unless a specific version is requested. To enable pre-releases globally, use --pre.",
        lines.join("\n")
    ))
}
