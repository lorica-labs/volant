// SPDX-License-Identifier: GPL-3.0-or-later
//! The controller's Python helper: one interpreter, alive for the whole run, that builds the
//! module payloads a Python task needs.
//!
//! ansible-core caches a module's zip by the module's name, so the first build of a module
//! costs 140 ms and every later one 2.8 ms - measured on ansible-core 2.19.12. A helper started
//! per task would pay the cold price every time, which is why this one is started once and
//! kept.
//!
//! What it hands back is **one** union zip for every module the run names, not one zip per
//! module: the entries the per-module zips share are byte-identical, so merging them is well
//! defined, and the union of the five modules this release ships is 631 KB against the 2.3 MB
//! the five separate zips add up to. The helper refuses a merge conflict rather than letting
//! the last module written win, because a blob whose `module_utils` came from an arbitrary
//! module would run something the playbook did not ask for.

mod action_helper;
mod builder;
mod reach;
mod types;
mod union_build;

pub(crate) use action_helper::{ActionHelper, Silence};
pub use builder::PythonBuilder;
pub(crate) use builder::{candidates, module_facts, resolved_from, resolved_json};
pub(crate) use reach::{Named, Reach, check_plugin_order, modules_for_run};
pub use reach::{is_python_module, module_path, set_module_path};
pub(crate) use types::HELPER;
#[cfg(test)]
pub use types::ModuleFacts;
pub use types::{Facts, ModulePayload, Natives, Resolved, Union, is_resolved_name, payload_key};
pub use union_build::union_and_helper;
#[cfg(test)]
pub use union_build::union_for;

#[cfg(test)]
mod tests;
