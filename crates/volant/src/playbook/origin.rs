// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a task was written, recorded while a file is parsed.

use saphyr::Yaml;

/// The task with nothing written on it, which is the outermost layer of every merge. It is
/// `PlayTask::empty` and not a derived zero: `loop_var` defaults to `item` and a derived
/// `Default` would leave it empty, which is a loop whose variable answers to no name.
/// Where a task was written: its file and the line and column its mapping starts at, both counted
/// from 1, as ansible-core's `Origin` names a task in a failure's error block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub path: std::sync::Arc<str>,
    pub line: usize,
    pub col: usize,
}

thread_local! {
    /// The file being parsed and where its mappings start, while one is.
    static ORIGINS: std::cell::RefCell<Option<(std::sync::Arc<str>, crate::yaml::Origins)>> =
        const { std::cell::RefCell::new(None) };
}

/// Keeps a file's origins readable by [`task_origin`] until dropped, then puts back those of
/// the file that was being parsed before, if any: an import parses inside its importer.
pub(super) struct OriginScope(Option<(std::sync::Arc<str>, crate::yaml::Origins)>);

impl OriginScope {
    pub(super) fn enter(source: &str, origins: crate::yaml::Origins) -> Self {
        let path = std::path::absolute(source)
            .map_or_else(|_| source.to_string(), |p| p.display().to_string());
        Self(ORIGINS.with(|o| o.replace(Some((path.into(), origins)))))
    }
}

impl Drop for OriginScope {
    fn drop(&mut self) {
        let previous = self.0.take();
        ORIGINS.with(|o| *o.borrow_mut() = previous);
    }
}

/// Where `value` was written, when it is a mapping entry's value in the file being parsed.
pub(super) fn value_origin(value: &Yaml<'_>) -> Option<Origin> {
    ORIGINS.with(|o| {
        o.borrow().as_ref().and_then(|(path, origins)| {
            origins.of_value(value).map(|(line, col)| Origin {
                path: path.clone(),
                line,
                col,
            })
        })
    })
}

/// Where the key of `value`'s entry was written, when it is a mapping entry's value in the file
/// being parsed.
pub(super) fn key_origin(value: &Yaml<'_>) -> Option<Origin> {
    ORIGINS.with(|o| {
        o.borrow().as_ref().and_then(|(path, origins)| {
            origins.of_key(value).map(|(line, col)| Origin {
                path: path.clone(),
                line,
                col,
            })
        })
    })
}

/// Where `map` was written, when it comes from the file being parsed.
pub(super) fn task_origin(map: &saphyr::Mapping<'_>) -> Option<Origin> {
    ORIGINS.with(|o| {
        o.borrow().as_ref().and_then(|(path, origins)| {
            origins.of(map).map(|(line, col)| Origin {
                path: path.clone(),
                line,
                col,
            })
        })
    })
}
