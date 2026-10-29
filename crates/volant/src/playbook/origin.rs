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

impl Origin {
    /// A value no file holds, as `SourceContext.from_value` shows it under `Origin:`: the
    /// reference's description of where it came from (`<adhoc 'ping' task>`, `<CLI option
    /// '-m'>`), a blank line, then the value as `str()` prints it, `textwrap.shorten` to 120
    /// characters. Line 0, so the error block prints it as it stands.
    pub fn described(description: &str, value: &str) -> Self {
        Origin {
            path: format!("{description}\n\n{}", crate::render::shorten(value, 120)).into(),
            line: 0,
            col: 0,
        }
    }
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

/// A load error the reference prints as an `[ERROR]` block at a place in a file, with no chain of
/// files and items in front of its message.
#[derive(Debug)]
pub(crate) struct Located {
    pub message: String,
    pub origin: Origin,
}

impl Located {
    /// What follows `[ERROR]: `: the message and the place, ending with the blank line a block of
    /// several lines ends with.
    pub(crate) fn text(&self) -> String {
        let block = crate::render::ErrorBlock {
            message: self.message.clone(),
            origin: Some(self.origin.clone()),
            unknown: None,
            causes: Vec::new(),
            lead: None,
            on_stderr: true,
        };
        let text = crate::render::block_text(&block);
        format!(
            "{}
",
            text.trim_start_matches("[ERROR]: ")
        )
    }
}

impl std::fmt::Display for Located {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Located {}

/// A play keyword whose value the reference cannot convert, as it refuses it when the play
/// starts: exit 4, `[ERROR]:`, then the keyword and the value's own sentence, each at the place
/// the value was written. Any other error, or one about a value no file holds, is kept as it is.
pub(super) fn keyword_refusal(yaml: &Yaml<'_>, err: anyhow::Error) -> anyhow::Error {
    let message = err.to_string();
    let Some((key, why)) = message
        .strip_prefix("Error processing keyword '")
        .and_then(|rest| rest.split_once("': "))
    else {
        return err;
    };
    let Some(origin) = crate::yaml::field(yaml, key).and_then(value_origin) else {
        return err;
    };
    let lead = format!("Error processing keyword '{key}'.");
    chained_refusal(&message, &lead, why, None, origin)
}

/// A load error the reference raises from another one, both about the value written at
/// `origin`: exit 4, `[ERROR]: message`, then `lead` and `cause` each at that place, `help`
/// under the cause.
pub(super) fn chained_refusal(
    message: &str,
    lead: &str,
    cause: &str,
    help: Option<&str>,
    origin: Origin,
) -> anyhow::Error {
    let block = crate::render::ErrorBlock {
        message: message.to_string(),
        origin: Some(origin.clone()),
        unknown: None,
        causes: vec![crate::render::Cause {
            msg: cause.to_string(),
            origin: Some(origin),
            help: help.map(str::to_string),
        }],
        lead: Some(lead.to_string()),
        on_stderr: true,
    };
    let text = crate::render::block_text(&block);
    crate::stats::Refusal::plain(4, format!("{}\n", text.trim_start_matches("[ERROR]: ")))
}

/// `message` at where `map` was written, or with no place when the file has none.
pub(super) fn located(message: String, map: &saphyr::Mapping<'_>) -> anyhow::Error {
    match task_origin(map) {
        Some(origin) => anyhow::Error::new(Located { message, origin }),
        None => anyhow::anyhow!(message),
    }
}
