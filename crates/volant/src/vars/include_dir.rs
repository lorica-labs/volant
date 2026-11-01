// SPDX-License-Identifier: GPL-3.0-or-later
//! The directory form of `include_vars`: which files of a tree it reads, in which order, and what
//! it answers when one of them cannot be used.
//!
//! The rules are those of ansible-core 2.19.12's `include_vars` action, measured: directories
//! are visited in the order of their full paths as text, the files of one directory in the order
//! of their names, hidden files and directories included, and a later file overrides an earlier
//! one key by key, whole values and not merged.

use std::path::Path;

use regex::Regex;
use serde_json::{Map, Value};

use super::load::{VaultFile, read_vars_document};

/// What `include_vars` was told about a directory, with the defaults the reference gives.
pub struct Scan {
    /// How many levels are read, the directory itself being the first. Zero reads them all.
    depth: f64,
    matcher: Option<Regex>,
    /// The patterns of `ignore_files`, or the sentence the reference's loop dies with when the
    /// value is not something it can iterate. Looked at only when a file is tested.
    ignore: Result<Vec<String>, String>,
    extensions: Vec<Value>,
    ignore_unknown: bool,
}

/// What a directory gave: the variables, the files read in order, and whether one of them was
/// vaulted.
pub struct Loaded {
    pub facts: Map<String, Value>,
    pub files: Vec<String>,
    pub encrypted: bool,
}

/// Why a directory could not be read.
pub enum Failure {
    /// The module answered a failure: a `message` beside the variables of the directories before
    /// the failing one and the files read so far.
    Returned {
        message: String,
        facts: Map<String, Value>,
        files: Vec<String>,
        encrypted: bool,
    },
    /// The action raised: the task fails with this text and no result of its own.
    Raised { text: String, chain: Chain },
}

/// How the reference's error block prints an exception the action raised.
pub enum Chain {
    /// On its own line.
    Alone,
    /// Behind `Task failed.` as a cause, because it was raised from inside another exception.
    Cause,
    /// As a cause that names the file it is about.
    CauseAt(String),
}

impl Scan {
    /// The scan for these options. `files_matching` is compiled here, as the reference compiles
    /// it before it looks at the directory; the error is Python's sentence for the pattern.
    pub fn new(
        files_matching: Option<&str>,
        ignore: Result<Vec<String>, String>,
        extensions: Vec<Value>,
        ignore_unknown: bool,
    ) -> Result<Scan, String> {
        let matcher = files_matching
            .map(|pattern| {
                Regex::new(pattern).map_err(|e| {
                    python_regex_error(pattern).unwrap_or_else(|| first_line(&e.to_string()))
                })
            })
            .transpose()?;
        Ok(Scan {
            depth: 0.0,
            matcher,
            ignore,
            extensions,
            ignore_unknown,
        })
    }

    /// The scan limited to `depth` levels.
    #[must_use]
    pub fn with_depth(mut self, depth: f64) -> Scan {
        self.depth = depth;
        self
    }

    /// Whether `name` has one of the extensions, compared as the reference compares them: the
    /// text after the last dot of the name, case and all.
    fn has_extension(&self, name: &str) -> bool {
        let ext = split_extension(name);
        self.extensions
            .iter()
            .any(|allowed| allowed.as_str() == Some(ext))
    }

    /// Whether an `ignore_files` pattern, anchored at the end of the name, finds `name`. Each
    /// pattern is compiled when it is reached, so one behind a match is never judged.
    fn ignored(&self, name: &str) -> Result<bool, Failure> {
        let patterns = self.ignore.as_ref().map_err(|text| Failure::Raised {
            text: text.clone(),
            chain: Chain::Alone,
        })?;
        for pattern in patterns {
            let invalid = || Failure::Raised {
                text: format!("Invalid regular expression: {pattern}"),
                chain: Chain::Cause,
            };
            let re = Regex::new(&format!("{pattern}$")).map_err(|_| invalid())?;
            if re.is_match(name) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn listing(&self) -> String {
        self.extensions
            .iter()
            .map(|e| e.as_str().map_or_else(|| e.to_string(), str::to_string))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Python's `os.path.join` for two parts: an absolute `tail` replaces `head`, and a `head` that
/// ends in a slash is not given a second one. Nothing is normalised, as nothing is in the
/// reference: `a/../b` stays what it was written.
pub fn join_path(head: &str, tail: &str) -> String {
    if tail.starts_with('/') || head.is_empty() {
        tail.to_string()
    } else if head.ends_with('/') {
        format!("{head}{tail}")
    } else {
        format!("{head}/{tail}")
    }
}

/// The extension of a file name as Python's `os.path.splitext` finds it, without its dot: the
/// part after the last dot, unless every character before that dot is a dot too.
fn split_extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) if name[..dot].chars().any(|c| c != '.') => &name[dot + 1..],
        _ => "",
    }
}

/// Every directory of the tree that `depth` allows, with its files, in the order the reference
/// reads them. Links to directories are followed, and a directory that cannot be listed is left
/// out, as `os.walk` leaves it out.
fn walk(top: &str, depth: f64) -> Vec<(String, Vec<String>)> {
    let mut found = Vec::new();
    let mut pending = vec![(top.to_string(), 1.0_f64)];
    while let Some((root, level)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let (mut dirs, mut files) = (Vec::new(), Vec::new());
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if std::fs::metadata(join_path(&root, &name)).is_ok_and(|m| m.is_dir()) {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
        if depth == 0.0 || level < depth {
            pending.extend(dirs.iter().map(|d| (join_path(&root, d), level + 1.0)));
        }
        if depth == 0.0 || level <= depth {
            files.sort();
            found.push((root, files));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Reads the directory `top`, which the caller has checked is one.
pub fn include_dir(top: &str, scan: &Scan) -> Result<Loaded, Failure> {
    let mut loaded = Loaded {
        facts: Map::new(),
        files: Vec::new(),
        encrypted: false,
    };
    for (root, names) in walk(top, scan.depth) {
        let mut here = Map::new();
        for name in names {
            let path = join_path(&root, &name);
            if scan.matcher.as_ref().is_some_and(|m| !m.is_match(&name)) {
                continue;
            }
            // `os.path.exists`: a link that leads nowhere is not a file to read.
            if !Path::new(&path).exists() || scan.ignored(&name)? {
                continue;
            }
            if !scan.has_extension(&name) {
                if scan.ignore_unknown {
                    continue;
                }
                return Err(Failure::Returned {
                    message: format!("{path} does not have a valid extension: {}", scan.listing()),
                    facts: loaded.facts,
                    files: loaded.files,
                    encrypted: loaded.encrypted,
                });
            }
            let document = read_vars_document(Path::new(&path)).map_err(|err| Failure::Raised {
                text: read_failure(&path, &err),
                chain: if err.downcast_ref::<VaultFile>().is_some() {
                    Chain::CauseAt(path.clone())
                } else {
                    Chain::Cause
                },
            })?;
            loaded.encrypted |= document.encrypted;
            match document.value {
                Value::Null => {}
                Value::Object(map) => here.extend(map),
                _ => {
                    return Err(Failure::Returned {
                        message: format!("{path} must be stored as a dictionary/hash"),
                        facts: loaded.facts,
                        files: loaded.files,
                        encrypted: loaded.encrypted,
                    });
                }
            }
            loaded.files.push(path);
        }
        loaded.facts.extend(here);
    }
    Ok(loaded)
}

/// The sentence the reference gives for a file it could not read: the vault's reason for a
/// vaulted one, Python's own wording of the system error for one it could not open, and
/// otherwise the parser's.
pub fn read_failure(path: &str, err: &anyhow::Error) -> String {
    if let Some(vault) = err.downcast_ref::<VaultFile>() {
        return vault.reason.clone();
    }
    if let Some(io) = err.chain().find_map(|e| e.downcast_ref::<std::io::Error>())
        && let Some(code) = io.raw_os_error()
    {
        let text = io.to_string();
        let reason = text
            .rsplit_once(" (os error ")
            .map_or(text.as_str(), |(reason, _)| reason);
        return format!(
            "An error occurred while trying to read the file '{path}': [Errno {code}] {reason}: '{path}'"
        );
    }
    format!("{err:#}")
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

/// Python's sentence for a pattern its `re` module refuses, for the three mistakes a pattern in
/// a playbook most often has: a parenthesis never closed or never opened, a character set never
/// closed, and a repeat with nothing in front of it. `None` for a pattern this finds nothing
/// wrong with. Positions count characters, as Python's do.
pub fn python_regex_error(pattern: &str) -> Option<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut open = Vec::new();
    let mut i = 0;
    let mut can_repeat = false;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 1;
                can_repeat = true;
            }
            '[' => {
                let start = i;
                i += 1;
                if chars.get(i) == Some(&'^') {
                    i += 1;
                }
                // A closing bracket right after the opening one is a member of the set.
                if chars.get(i) == Some(&']') {
                    i += 1;
                }
                while i < chars.len() && chars[i] != ']' {
                    i += if chars[i] == '\\' { 2 } else { 1 };
                }
                if i >= chars.len() {
                    return Some(format!("unterminated character set at position {start}"));
                }
                can_repeat = true;
            }
            '(' => {
                open.push(i);
                can_repeat = false;
                // The `?` of `(?:` and `(?P<name>` opens a group, it repeats nothing.
                if chars.get(i + 1) == Some(&'?') {
                    i += 1;
                }
            }
            ')' => {
                if open.pop().is_none() {
                    return Some(format!("unbalanced parenthesis at position {i}"));
                }
                can_repeat = true;
            }
            '*' | '+' | '?' if !can_repeat => {
                return Some(format!("nothing to repeat at position {i}"));
            }
            '|' | '^' => can_repeat = false,
            _ => can_repeat = true,
        }
        i += 1;
    }
    open.last()
        .map(|at| format!("missing ), unterminated subpattern at position {at}"))
}
