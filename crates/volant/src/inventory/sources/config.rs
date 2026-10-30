// SPDX-License-Identifier: GPL-3.0-or-later
//! The `[inventory]` settings the chain of inventory plugins reads.

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
    /// The script plugin's `always_show_stderr`: what a successful script wrote on its standard
    /// error is shown as an `[ERROR]:` line.
    pub script_stderr: bool,
    /// `export`: `ansible-inventory` lists as with `--export`.
    pub export: bool,
    /// Why `-e` cannot be read, as a message and its `Origin:`: the reference's plugins read
    /// `-e` before their source, so each one that takes a source fails with it.
    pub extra_vars_error: Option<(String, Option<String>)>,
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
            script_stderr: true,
            extra_vars_error: None,
            export: false,
        }
    }
}
