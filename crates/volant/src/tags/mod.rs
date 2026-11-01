// SPDX-License-Identifier: GPL-3.0-or-later
//! Data tags, as ansible-core 2.19 puts them on values: where a value came from (`Origin`),
//! whether it may be rendered as a template (`TrustedAsTemplate`), its vault provenance
//! (`VaultedValue`, `SourceWasEncrypted`) and a module's deprecation of it (`Deprecated`).
//!
//! A value's tags are a [`TagSet`], interned once and named by a [`TagSetId`]; a [`TagTree`]
//! beside a `serde_json::Value` holds the ids of the parts of it that carry tags.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

mod tree;

pub use tree::{Children, TagMap, TagTree, merge};

/// An interned origin: a path with its line and column, or a description such as
/// `<environment variable 'HOME'>`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct OriginId(u32);

/// An interned [`TagSet`]. [`TagSetId::NONE`] is the empty set; trees never store it on a leaf.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TagSetId(u32);

impl TagSetId {
    pub const NONE: TagSetId = TagSetId(0);

    pub fn is_none(self) -> bool {
        self == Self::NONE
    }
}

/// The tags one value carries. `bool` and null values carry none, as in Python.
#[derive(Clone, PartialEq, Eq, Hash, Default, Debug)]
pub struct TagSet {
    pub origin: Option<OriginId>,
    /// `TrustedAsTemplate`: the string may be rendered as a template.
    pub trusted: bool,
    /// `SourceWasEncrypted`: loaded from a vault-encrypted file.
    pub source_encrypted: bool,
    /// `VaultedValue`: the ciphertext the value was decrypted from.
    pub vaulted: Option<Arc<str>>,
    pub deprecated: Option<Arc<Deprecated>>,
}

/// The `Deprecated` tag a module puts on a value it returns.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Deprecated {
    pub msg: String,
    pub help_text: Option<String>,
    pub date: Option<String>,
    pub version: Option<String>,
    /// The deprecator's resolved name and type.
    pub deprecator: Option<(String, String)>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Origin {
    path: Option<Arc<str>>,
    description: Option<Arc<str>>,
    line: u32,
    col: u32,
}

/// Every origin and tag set of the process. They are never evicted: memory grows with the number
/// of distinct origins and tag sets the run meets (a 200-host run with a large role: well under
/// a megabyte). A per-run arena dropped with the run would bound it.
struct Tables {
    strings: HashSet<Arc<str>>,
    origins: Vec<Origin>,
    origin_ids: HashMap<Origin, u32>,
    sets: Vec<TagSet>,
    set_ids: HashMap<TagSet, u32>,
}

static TABLES: LazyLock<RwLock<Tables>> = LazyLock::new(|| {
    RwLock::new(Tables {
        strings: HashSet::new(),
        origins: Vec::new(),
        origin_ids: HashMap::new(),
        sets: vec![TagSet::default()],
        set_ids: HashMap::from([(TagSet::default(), 0)]),
    })
});

fn tables() -> std::sync::RwLockReadGuard<'static, Tables> {
    TABLES.read().unwrap_or_else(PoisonError::into_inner)
}

fn tables_mut() -> std::sync::RwLockWriteGuard<'static, Tables> {
    TABLES.write().unwrap_or_else(PoisonError::into_inner)
}

impl Tables {
    fn string(&mut self, text: &str) -> Arc<str> {
        if let Some(found) = self.strings.get(text) {
            return found.clone();
        }
        let interned: Arc<str> = Arc::from(text);
        self.strings.insert(interned.clone());
        interned
    }

    fn origin(
        &mut self,
        path: Option<&str>,
        description: Option<&str>,
        line: u32,
        col: u32,
    ) -> OriginId {
        let origin = Origin {
            path: path.map(|p| self.string(p)),
            description: description.map(|d| self.string(d)),
            line,
            col,
        };
        if let Some(&id) = self.origin_ids.get(&origin) {
            return OriginId(id);
        }
        let id = u32::try_from(self.origins.len()).expect("fewer than 2^32 origins");
        self.origins.push(origin.clone());
        self.origin_ids.insert(origin, id);
        OriginId(id)
    }
}

/// The origin `path:line:col`. A line or column of 0 is unknown and left out of the text.
pub fn origin_at(path: &str, line: u32, col: u32) -> OriginId {
    tables_mut().origin(Some(path), None, line, col)
}

/// An origin known only by its description, such as `<CLI option 'extra_vars'>`.
pub fn origin_described(description: &str) -> OriginId {
    tables_mut().origin(None, Some(description), 0, 0)
}

/// `str(Origin)` of ansible-core: the path or the description, then `:line` and `:col` when known.
pub fn origin_text(id: OriginId) -> String {
    let tables = tables();
    let origin = &tables.origins[id.0 as usize];
    let mut text = origin
        .path
        .as_deref()
        .or(origin.description.as_deref())
        .unwrap_or_default()
        .to_string();
    if origin.line > 0 {
        _ = write!(text, ":{}", origin.line);
        if origin.col > 0 {
            _ = write!(text, ":{}", origin.col);
        }
    }
    text
}

/// The path, line and column of an origin that has a path.
pub fn origin_place(id: OriginId) -> Option<(Arc<str>, u32, u32)> {
    let tables = tables();
    let origin = &tables.origins[id.0 as usize];
    origin
        .path
        .clone()
        .map(|path| (path, origin.line, origin.col))
}

pub fn intern(set: TagSet) -> TagSetId {
    if let Some(&id) = tables().set_ids.get(&set) {
        return TagSetId(id);
    }
    let mut tables = tables_mut();
    if let Some(&id) = tables.set_ids.get(&set) {
        return TagSetId(id);
    }
    let id = u32::try_from(tables.sets.len()).expect("fewer than 2^32 tag sets");
    tables.sets.push(set.clone());
    tables.set_ids.insert(set, id);
    TagSetId(id)
}

pub fn get(id: TagSetId) -> TagSet {
    tables().sets[id.0 as usize].clone()
}

/// The names of the tags in a set, sorted as `ansible._protomatter.tag_names` sorts them.
pub fn names(id: TagSetId) -> Vec<&'static str> {
    let set = get(id);
    let mut names = Vec::new();
    if set.deprecated.is_some() {
        names.push("Deprecated");
    }
    if set.origin.is_some() {
        names.push("Origin");
    }
    if set.source_encrypted {
        names.push("SourceWasEncrypted");
    }
    if set.trusted {
        names.push("TrustedAsTemplate");
    }
    if set.vaulted.is_some() {
        names.push("VaultedValue");
    }
    names
}

#[cfg(test)]
mod tests;
