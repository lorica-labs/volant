// SPDX-License-Identifier: GPL-3.0-or-later
//! The inventory manager's side of ansible-core 2.19.12 (`inventory/manager.py`): which sources a
//! run reads, which plugin takes each one, and what is said when none does.

use std::cell::{OnceCell, RefCell};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};

use regex::Regex;

use super::{Inventory, PLUGINS};
use crate::bridge::{Bridge, PluginKind};
use crate::template::{Templar, bridged_name};

/// What a run reads hosts from: a path, made absolute the way the reference's `unfrackpath`
/// makes it, or a host list when the text holds a comma.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    text: String,
}

/// The source the reference reads when none is given, and the one it says nothing about when
/// it is missing.
const DEFAULT_SOURCE: &str = "/etc/ansible/hosts";

impl Source {
    pub fn new(given: &str) -> Self {
        let text = if given.contains(',') || given.is_empty() {
            given.to_string()
        } else {
            unfrack(given).to_string_lossy().into_owned()
        };
        Self { text }
    }

    /// The reference's default source, `/etc/ansible/hosts`.
    pub fn default_source() -> Self {
        Self::new(DEFAULT_SOURCE)
    }

    /// The source as the reference's messages show it.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn path(&self) -> &Path {
        Path::new(&self.text)
    }

    /// The base `verify_file`: the path exists and can be read.
    pub fn readable_file(&self) -> bool {
        self.path().exists() && std::fs::File::open(self.path()).is_ok()
    }

    /// The directory whose `group_vars` and `host_vars` go with this source: a directory source
    /// itself, a file's parent. A host list has none.
    pub fn vars_dir(&self) -> Option<PathBuf> {
        let path = self.path();
        if self.text.contains(',') && !path.exists() {
            return None;
        }
        if path.is_dir() {
            return Some(path.to_path_buf());
        }
        path.parent().map(Path::to_path_buf)
    }
}

/// `unfrackpath(path, follow=False)`: `~` expanded, made absolute, `.` and `..` folded away
/// without resolving links.
fn unfrack(given: &str) -> PathBuf {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    let expanded = match (given.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(given),
    };
    let absolute = std::path::absolute(&expanded).unwrap_or(expanded);
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The `[inventory]` settings the chain reads, with the reference's defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConfig {
    /// `enable_plugins`, in the order the plugins are tried.
    pub enabled: Vec<String>,
    /// `ignore_extensions`: a directory entry ending with one of these is skipped.
    pub ignore_extensions: Vec<String>,
    /// `ignore_patterns`: a directory entry one of these regular expressions finds is skipped.
    pub ignore_patterns: Vec<String>,
    /// `unparsed_is_failed`: the run fails when no source at all could be read.
    pub unparsed_is_failed: bool,
    /// `any_unparsed_is_failed`: the run fails at the first source no plugin could read.
    pub any_unparsed_is_failed: bool,
}

impl Default for PluginConfig {
    fn default() -> Self {
        let words = |list: &[&str]| list.iter().map(ToString::to_string).collect();
        Self {
            enabled: words(&["host_list", "script", "auto", "yaml", "ini", "toml"]),
            ignore_extensions: words(&[
                ".pyc", ".pyo", ".swp", ".bak", "~", ".rpm", ".md", ".txt", ".rst", ".orig",
                ".cfg", ".retry",
            ]),
            ignore_patterns: Vec::new(),
            unparsed_is_failed: false,
            any_unparsed_is_failed: false,
        }
    }
}

/// Why a plugin that took a source could not read it. `origin` is the reference's `Origin:`
/// line; without one, the chain names the plugin and the source.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginError {
    pub message: String,
    pub origin: Option<String>,
    /// What this error was raised from: its message and its `Origin:`, a second block of the
    /// reference's chain; the `Origin:` line is left out when the cause has none.
    pub cause: Option<(String, Option<String>)>,
    /// A plugin this release does not have yet: refused by name when no other plugin reads the
    /// source, rather than reported as a source that could not be parsed.
    pub unsupported: bool,
}

impl PluginError {
    pub fn unsupported(plugin: &str) -> Self {
        Self {
            message: format!("the {plugin} inventory plugin is not supported yet"),
            origin: None,
            cause: None,
            unsupported: true,
        }
    }
}

/// What a plugin may use while it reads a source: the chain's settings, and the lines it says.
pub struct ParseCx<'a> {
    pub config: &'a PluginConfig,
    notes: RefCell<Vec<Note>>,
    start: &'a dyn Fn() -> Arc<Bridge>,
    helper: OnceCell<Arc<Bridge>>,
}

impl<'a> ParseCx<'a> {
    pub(super) fn new(config: &'a PluginConfig, start: &'a dyn Fn() -> Arc<Bridge>) -> Self {
        Self {
            config,
            notes: RefCell::new(Vec::new()),
            start,
            helper: OnceCell::new(),
        }
    }

    /// The Python helper that runs collection plugins, started by the first one that needs it.
    pub fn helper(&self) -> &Arc<Bridge> {
        self.helper.get_or_init(self.start)
    }

    /// A templar for the Jinja text in `config`. It reaches the Python helper only when that text
    /// names a filter or test the engine lacks: starting the helper is not free, and a config of
    /// core filters must not start it. Names passed as strings (`map('f')`) are not looked for.
    pub fn templar_for(&self, base: PathBuf, config: &serde_json::Value) -> Templar {
        static NAMES: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"\|\s*([A-Za-z_][\w.]*)|\bis\s+(?:not\s+)?([A-Za-z_][\w.]*)")
                .expect("static regex")
        });
        fn strings<'v>(value: &'v serde_json::Value, out: &mut Vec<&'v str>) {
            match value {
                serde_json::Value::String(s) => out.push(s),
                serde_json::Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
                serde_json::Value::Object(o) => o.values().for_each(|v| strings(v, out)),
                _ => {}
            }
        }
        let mut texts = Vec::new();
        strings(config, &mut texts);
        let lacking = texts.iter().any(|text| {
            NAMES.captures_iter(text).any(|c| {
                let (kind, name) = match (c.get(1), c.get(2)) {
                    (Some(n), _) => (PluginKind::Filter, n.as_str()),
                    (_, Some(n)) => (PluginKind::Test, n.as_str()),
                    _ => return false,
                };
                bridged_name(kind, name)
            })
        });
        Templar::with_bridge(base, lacking.then(|| Arc::clone(self.helper())))
    }

    /// A line shown from verbosity `level` on.
    pub fn verbose(&self, level: u8, text: String) {
        self.notes.borrow_mut().push(Note::Verbose(level, text));
    }

    /// A `[WARNING]:` line the run has not shown yet.
    pub fn warn_once(&self, text: String) {
        let seen = self
            .notes
            .borrow()
            .iter()
            .any(|n| matches!(n, Note::Warning(t) if *t == text));
        if !seen {
            self.warn(text);
        }
    }

    /// A `[WARNING]:` line.
    pub fn warn(&self, text: String) {
        self.notes.borrow_mut().push(Note::Warning(text));
    }

    /// A `[DEPRECATION WARNING]:` block.
    pub fn deprecate(&self, text: String) {
        self.notes.borrow_mut().push(Note::Deprecation(text));
    }
}

/// One inventory plugin: whether it takes a source, and reading it into the inventory. What it
/// added before an error stays, as the reference's partial state does.
pub trait InventoryPlugin: Sync {
    fn name(&self) -> &'static str;
    fn verify(&self, source: &Source) -> bool;
    fn parse(&self, source: &Source, into: &mut Inventory, cx: &ParseCx)
    -> Result<(), PluginError>;
}

/// Something the chain says, in the order the reference says it.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    Verbose(u8, String),
    Warning(String),
    /// A `[DEPRECATION WARNING]:` block, its `Origin:` line included.
    Deprecation(String),
}

/// The end of reading every source: the run stops with `code` after printing `[ERROR]: message`.
#[derive(Debug, Clone, PartialEq)]
pub struct Fatal {
    pub code: i32,
    pub message: String,
}

/// The inventory every source made, what was said while reading them, whether at least one
/// source was read, and the error that ends the run, if any.
#[derive(Debug, Default)]
pub struct Loaded {
    pub inventory: Inventory,
    pub notes: Vec<Note>,
    pub parsed: bool,
    pub error: Option<Fatal>,
}

/// Every source, in order, into one inventory: a later source adds its hosts and groups after
/// the earlier ones and wins a variable both set.
pub fn load_sources(sources: &[Source], config: &PluginConfig) -> Loaded {
    load_sources_with(sources, config, &|| Bridge::new(Vec::new()))
}

/// [`load_sources`] with the way a Python helper is made for the collection plugins.
pub fn load_sources_with(
    sources: &[Source],
    config: &PluginConfig,
    start: &dyn Fn() -> Arc<Bridge>,
) -> Loaded {
    let mut loaded = Loaded::default();
    let cx = ParseCx::new(config, start);
    let mut plugins: Vec<&dyn InventoryPlugin> = Vec::new();
    for name in &config.enabled {
        let short = name.strip_prefix("ansible.builtin.").unwrap_or(name);
        match PLUGINS.iter().find(|p| p.name() == short) {
            Some(plugin) => plugins.push(*plugin),
            None => cx.warn(format!("Failed to load inventory plugin, skipping {name}")),
        }
    }
    let result = match (plugins.is_empty(), ignored_names(config)) {
        (true, _) => Err(Fatal {
            code: 1,
            message: "No inventory plugins available to generate inventory, make sure you have \
                      at least one enabled."
                .to_string(),
        }),
        (false, Err(fatal)) => Err(fatal),
        (false, Ok(ignored)) => {
            let chain = Chain {
                plugins: &plugins,
                ignored: &ignored,
                cx: &cx,
            };
            sources
                .iter()
                .filter(|s| !s.text.is_empty())
                .try_fold(false, |parsed, source| {
                    Ok(chain.source(source, &mut loaded.inventory)? || parsed)
                })
        }
    };
    match result {
        Ok(true) => {
            loaded.parsed = true;
            loaded.inventory.reconcile();
        }
        // What a source that failed part-way added stays only when a later source is read: the
        // reference attaches groups to `all` once something parsed, so with nothing parsed the
        // run sees no host at all.
        Ok(false) if config.unparsed_is_failed => {
            loaded.inventory = Inventory::default();
            loaded.error = Some(Fatal {
                code: 1,
                message: "No inventory was parsed, please check your configuration and options."
                    .to_string(),
            });
        }
        Ok(false) => {
            loaded.inventory = Inventory::default();
            cx.warn("No inventory was parsed, only implicit localhost is available".into());
        }
        Err(fatal) => loaded.error = Some(fatal),
    }
    loaded.notes = cx.notes.into_inner();
    loaded
}

/// The reference's `IGNORED`: dot files, the variable directories, the ignored patterns and
/// extensions, searched anywhere in an entry's name.
fn ignored_names(config: &PluginConfig) -> Result<Regex, Fatal> {
    let mut parts: Vec<String> = ["^\\.", "^host_vars$", "^group_vars$", "^vars_plugins$"]
        .map(String::from)
        .to_vec();
    for pattern in &config.ignore_patterns {
        Regex::new(pattern).map_err(|e| Fatal {
            code: 1,
            message: format!("Invalid inventory ignore pattern '{pattern}': {e}"),
        })?;
        parts.push(format!("(?:{pattern})"));
    }
    parts.extend(
        config
            .ignore_extensions
            .iter()
            .map(|e| format!("{}$", regex::escape(e))),
    );
    Regex::new(&parts.join("|")).map_err(|e| Fatal {
        code: 1,
        message: format!("Invalid inventory ignore list: {e}"),
    })
}

struct Chain<'a, 'c> {
    plugins: &'a [&'a dyn InventoryPlugin],
    ignored: &'a Regex,
    cx: &'a ParseCx<'c>,
}

impl Chain<'_, '_> {
    /// `parse_source`: a directory's entries in sorted order, anything else offered to each
    /// plugin until one reads it. Whether something was read.
    fn source(&self, source: &Source, into: &mut Inventory) -> Result<bool, Fatal> {
        let mut parsed = false;
        let mut failures = Vec::new();
        if source.path().is_dir() {
            let mut names: Vec<_> = std::fs::read_dir(source.path())
                .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
                .unwrap_or_default();
            names.sort();
            for name in names {
                if self.ignored.is_match(&name.to_string_lossy()) {
                    continue;
                }
                let child = Source {
                    text: source.path().join(&name).to_string_lossy().into_owned(),
                };
                parsed |= self.source(&child, into)?;
            }
        } else {
            for plugin in self.plugins {
                if !plugin.verify(source) {
                    self.cx.verbose(
                        3,
                        format!(
                            "{} declined parsing {} as it did not pass its verify_file() method",
                            plugin.name(),
                            source.text
                        ),
                    );
                    continue;
                }
                match plugin.parse(source, into, self.cx) {
                    Ok(()) => {
                        parsed = true;
                        self.cx.verbose(
                            3,
                            format!(
                                "Parsed {} inventory source with {} plugin",
                                source.text,
                                plugin.name()
                            ),
                        );
                        break;
                    }
                    Err(err) => failures.push((plugin.name(), err)),
                }
            }
        }
        if parsed || (source.text == DEFAULT_SOURCE && !source.path().exists()) {
            return Ok(parsed);
        }
        let missing: Vec<&str> = failures
            .iter()
            .filter(|(_, e)| e.unsupported)
            .map(|(name, _)| *name)
            .collect();
        if !missing.is_empty() {
            return Err(Fatal {
                code: 4,
                message: format!(
                    "inventory source {} needs the {} inventory plugin, which is not supported \
                     yet",
                    source.text,
                    missing.join(" or ")
                ),
            });
        }
        for (plugin, err) in failures {
            let origin = err.origin.unwrap_or_else(|| {
                format!(
                    "<inventory plugin '{plugin}' with source '{}'>",
                    source.text
                )
            });
            let message = err.message;
            let (first, more) = match err.cause {
                Some((cause, cause_origin)) => (
                    format!("{}: {cause}", message.trim_end_matches('.')),
                    format!(
                        "\n<<< caused by >>>\n\n{cause}\n{}",
                        cause_origin.map_or_else(String::new, |o| format!("Origin: {o}\n"))
                    ),
                ),
                None => (message.clone(), String::new()),
            };
            self.cx.warn(format!(
                "Failed to parse inventory with '{plugin}' plugin: {first}\n\nFailed to parse \
                 inventory with '{plugin}' plugin.\n\n<<< caused by >>>\n\n{message}\nOrigin: \
                 {origin}\n{more}"
            ));
        }
        if self.cx.config.any_unparsed_is_failed {
            return Err(Fatal {
                code: 1,
                message: format!(
                    "Completely failed to parse inventory source {}",
                    source.text
                ),
            });
        }
        self.cx.warn(format!(
            "Unable to parse {} as an inventory source",
            source.text
        ));
        Ok(false)
    }
}

/// `verify_file` of a plugin that reads files: the base check plus the extensions it takes,
/// `None` standing for a name without one.
pub(super) fn file_with_extension(source: &Source, extensions: &[Option<&str>]) -> bool {
    let ext = source.path().extension().map(|e| e.to_string_lossy());
    source.readable_file() && extensions.contains(&ext.as_deref())
}

#[cfg(unix)]
pub(super) fn executable(source: &Source) -> bool {
    use std::os::unix::fs::PermissionsExt;
    source.readable_file()
        && std::fs::metadata(source.path()).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub(super) fn executable(_source: &Source) -> bool {
    false
}

#[cfg(test)]
mod tests;
