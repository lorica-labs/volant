// SPDX-License-Identifier: GPL-3.0-or-later
//! Jinja2 templating the way ansible-core 2.19 does it: strict about undefined variables, and a
//! template that is one expression yields that expression's value, not its text.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

mod bridged;
mod context;
mod env;
mod file;
mod filters;
pub(crate) use filters::python_json;
mod lookups;
mod templar;
mod test_plugins;
mod vm;
mod yaml_dump;

pub use context::Vars;
use context::{Context, TaintSink, context_of};
pub(crate) use env::{
    add_filter, add_test, add_text_filter, holds_undefined, truthy, undefined_through,
};
use env::{any_undefined, environment, type_name};
pub use templar::Templar;
use vm::{convert_error, holds_template, render_str};

pub use bridged::{bridged_name, not_sent_reason};
pub use file::FileRender;
pub use lookups::supported as lookup_supported;

/// Prefix of every error raised because a variable had no value, the one template failure
/// Ansible treats as recoverable in places such as `vars_files`.
const UNDEFINED: &str = "The task includes an option with an undefined variable.";

/// What [`Templar::render_map_tainted`] puts before a plugin's failure, then the key in quotes.
pub(crate) const RESOLVING: &str = "Error while resolving value for ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError(pub String);

impl TemplateError {
    /// Whether the render failed on a variable that had no value, as opposed to a syntax error,
    /// an unknown filter or an illegal operation.
    pub fn is_undefined(&self) -> bool {
        self.0.starts_with(UNDEFINED)
    }
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TemplateError {}

/// Set while a render pass reads a name whose value came from a managed host rather than from
/// the playbook. A result built from such a name is data, so the extra passes below do not run
/// on it and an expression that arrived in a command's output is never evaluated here.
pub(crate) type Tainted = Arc<AtomicBool>;

/// The name `Context` answers with the render's taint sink. Two colons cannot appear in a Jinja
/// identifier, so no playbook can write it as a variable or shadow it; `lookup('vars', ...)`
/// reaches it, and hands back an object whose only method marks the render as data.
pub(crate) const TAINT_KEY: &str = "volant::tainted";

/// The name `Context` answers with itself, for `lookup('template')`, which renders a file
/// against the same variables and the same trust as the render that called it. Unreachable from
/// a playbook for the same reason as `TAINT_KEY`; through `lookup('vars', ...)` it hands back
/// the root context, whose every read goes through `get_value` and taints as a bare read does.
pub(crate) const CONTEXT_KEY: &str = "volant::context";

/// The global every `~` is compiled to call, see [`vm::route_calls`]. Unwritable in Jinja for the
/// same reason as `TAINT_KEY`, and `Context` answers nothing for it, so no variable shadows it.
/// What `route_calls` puts in place of the name in `NAME is vault_encrypted`, before the name.
pub(crate) const VAULT_NAME: &str = "volant::vault_name:";

const CONCAT_KEY: &str = "volant::concat";
const ITEM_KEY: &str = "volant::item";
/// Prefix of the name an attribute lookup is routed through; the context answers it with a
/// function that reads that attribute.
const ATTR_KEY: &str = "volant::attr:";

/// The global a call to `query` or `q` is compiled to call, see [`vm::route_calls`]. Not a global of
/// either name: measured on ansible-core 2.19.12, `q is defined` is false on a host with no
/// variable `q`.
pub(crate) const QUERY_KEY: &str = "volant::query";

#[cfg(test)]
mod tests;
