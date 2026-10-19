// SPDX-License-Identifier: GPL-3.0-or-later
//! Jinja2 templating the way ansible-core 2.19 does it: strict about undefined variables, and a
//! template that is one expression yields that expression's value, not its text.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

mod bigint;
mod bridged;
mod clock;
mod context;
mod env;
mod file;
mod filters;
pub(crate) use filters::python_json;
mod globals;
mod lipsum;
mod lookups;
mod once;
mod one_pass;
mod pymethods;
mod resolve;
mod templar;
mod test_plugins;
mod vm;
mod yaml_dump;

pub use context::{ALLITEMS_KEY, Vars, complete_loop, with_allitems};
use context::{Context, TaintSink, context_of, take_failure};
pub(crate) use env::{
    add_filter, add_test, add_text_filter, holds_undefined, truthy, undefined_through,
};
use env::{any_undefined, environment, type_name};
pub use templar::Templar;
pub(crate) use vm::holds_template;
use vm::{convert_error, render_str};

pub use bridged::{bridged_name, not_sent_reason};
pub use file::{FileRender, Syntax};
pub use lookups::supported as lookup_supported;

/// Prefix of every error raised because a variable had no value, the one template failure
/// Ansible treats as recoverable in places such as `vars_files`.
pub(crate) const UNDEFINED: &str = "The task includes an option with an undefined variable.";

/// What the reference says when a variable's value is a template that reads the variable again,
/// directly or through others.
pub(crate) const RECURSION: &str =
    "Recursive loop detected in template: maximum recursion depth exceeded";

/// What [`Templar::render_map_tainted`] puts before a plugin's failure, then the key in quotes.
pub(crate) const RESOLVING: &str = "Error while resolving value for ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError(pub String);

impl TemplateError {
    /// Whether the render failed on a variable that had no value, as opposed to a syntax error,
    /// an unknown filter or an illegal operation.
    pub fn is_undefined(&self) -> bool {
        self.0.starts_with(UNDEFINED)
            || self
                .0
                .strip_prefix("Task failed: '")
                .is_some_and(|rest| rest.ends_with("' is undefined"))
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

/// Among a render's untrusted names when a host name came from a managed host: listing the
/// hosts of `hostvars` then taints the render, while reading one host is `untrusted_hosts`' case,
/// so a nested template of a trusted host still renders. Unwritable for the same reason as
/// `TAINT_KEY`.
pub(crate) const HOSTVARS_KEYS: &str = "volant::hostvars_keys";

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
/// `ATTR_KEY` for a template that changes a list or mapping in place: what it reads is the
/// mutable form of the value, see [`pymethods::mutable`].
const MATTR_KEY: &str = "volant::mattr:";
/// The globals `%` is compiled to call: the second when the right side was written as a tuple,
/// see [`vm::route_calls`].
const REM_KEY: &str = "volant::rem";
const REMT_KEY: &str = "volant::remt";
/// Prefix of the name a list or mapping literal is routed through in such a template, then the
/// literal as JSON: minijinja folds a literal of constants into one shared value, and a template
/// that changes a list needs a new one each time the literal runs.
const CONST_KEY: &str = "volant::const:";
/// Prefix of the name a variable read is routed through in such a template.
const VAR_KEY: &str = "volant::var:";
/// `ITEM_KEY` for such a template.
const MITEM_KEY: &str = "volant::mitem";
/// What a list or mapping written out in such a template is built by, so that it can change.
const MKLIST_KEY: &str = "volant::mklist";
const MKMAP_KEY: &str = "volant::mkmap";

/// The global a call to `query` or `q` is compiled to call, see [`vm::route_calls`]. Not a global of
/// either name: measured on ansible-core 2.19.12, `q is defined` is false on a host with no
/// variable `q`.
pub(crate) const QUERY_KEY: &str = "volant::query";

#[cfg(test)]
mod tests;
