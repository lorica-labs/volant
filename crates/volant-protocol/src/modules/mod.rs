// SPDX-License-Identifier: GPL-3.0-or-later
//! The native modules and what the controller needs to know about them before the agent runs
//! them. Written once here; the agent's implementation table and the documentation are
//! checked against it.

mod action_plugins;
mod builtin;
mod docs;
mod local;
mod native;
mod spec;

pub use action_plugins::ACTION_PLUGINS;
pub use builtin::{BUILTIN_MODULES, Route, builtin_route, is_builtin, is_known};
pub use docs::documentation_table;
pub use local::{
    ASSERT, DEBUG, FAIL, IMPORT_MODULES, INCLUDE_MODULES, INCLUDE_VARS, LOCAL_MODULES, PAUSE,
    SET_FACT, VALIDATE_ARGUMENT_SPEC, import_module, include_module,
};
pub use native::{
    ASYNC_STATUS, CHECK_MODE_ARG, COMMAND, DIFF_ARG, NATIVE_CANDIDATES, NATIVE_ENABLED,
    NATIVE_MODULES, NO_LOG_ARG, RAW, SHELL,
};
use spec::honoured;
pub use spec::{ArgStatus, ModuleArg, ModuleSpec, arg_bool};

/// `ansible.builtin.` and `ansible.legacy.` name the same modules as the bare name does.
/// Other collections are returned whole: `community.general.command` is not our `command`.
pub fn short_name(module: &str) -> &str {
    module
        .strip_prefix("ansible.builtin.")
        .or_else(|| module.strip_prefix("ansible.legacy."))
        .unwrap_or(module)
}

/// The builtin names ansible-core routes to another builtin, as `(name, target)`. Measured on
/// ansible-core 2.19.12 from `plugin_routing` in `ansible_builtin_runtime.yml`: `yum` is a
/// redirect to the `dnf` action and has no module or documentation of its own. The test-only
/// redirects of that file are left out.
pub const BUILTIN_REDIRECTS: &[(&str, &str)] = &[("yum", "dnf")];

/// The builtin a name runs: [`short_name`], then the redirect ansible-core keeps for it.
/// `module_defaults` and the facts a task reads or writes follow the target, while the name
/// a task wrote stays what messages and `ansible_failed_task` show.
pub fn resolved_name(module: &str) -> &str {
    let short = short_name(module);
    BUILTIN_REDIRECTS
        .iter()
        .find(|(name, _)| *name == short)
        .map_or(short, |(_, target)| target)
}

pub fn native(module: &str) -> Option<&'static ModuleSpec> {
    let short = short_name(module);
    NATIVE_MODULES.iter().find(|m| m.name == short)
}

pub fn local(module: &str) -> Option<&'static ModuleSpec> {
    let short = short_name(module);
    LOCAL_MODULES.iter().find(|m| m.name == short)
}

#[cfg(test)]
mod tests;
