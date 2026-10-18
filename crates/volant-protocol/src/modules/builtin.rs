// SPDX-License-Identifier: GPL-3.0-or-later

//! Every builtin module of ansible-core, and where each one runs.

use super::{
    ACTION_PLUGINS, NATIVE_ENABLED, import_module, include_module, local, native, short_name,
};

/// Every module `ansible.builtin` ships in ansible-core 2.19.12, as `ansible-doc -l -t module
/// ansible.builtin` lists them, sorted and short-named. A playbook naming one of these and
/// none of `NATIVE_MODULES` or `LOCAL_MODULES` is a playbook this release cannot run yet,
/// which the operator needs to hear differently from a name that resolves to no module at all:
/// the first waits on us, the second is a typo.
pub const BUILTIN_MODULES: &[&str] = &[
    "add_host",
    "apt",
    "apt_key",
    "apt_repository",
    "assemble",
    "assert",
    "async_status",
    "blockinfile",
    "command",
    "copy",
    "cron",
    "deb822_repository",
    "debconf",
    "debug",
    "dnf",
    "dnf5",
    "dpkg_selections",
    "expect",
    "fail",
    "fetch",
    "file",
    "find",
    "gather_facts",
    "get_url",
    "getent",
    "git",
    "group",
    "group_by",
    "hostname",
    "import_playbook",
    "import_role",
    "import_tasks",
    "include_role",
    "include_tasks",
    "include_vars",
    "iptables",
    "known_hosts",
    "lineinfile",
    "meta",
    "mount_facts",
    "package",
    "package_facts",
    "pause",
    "ping",
    "pip",
    "raw",
    "reboot",
    "replace",
    "rpm_key",
    "script",
    "service",
    "service_facts",
    "set_fact",
    "set_stats",
    "setup",
    "shell",
    "slurp",
    "stat",
    "subversion",
    "systemd",
    "systemd_service",
    "sysvinit",
    "tempfile",
    "template",
    "unarchive",
    "uri",
    "user",
    "validate_argument_spec",
    "wait_for",
    "wait_for_connection",
    "yum_repository",
];

/// Whether `module` names a builtin, bare or under the two prefixes that mean the same thing.
/// Another collection's module is never one, however familiar the short name looks: if
/// `community.general.command` were taken for a builtin, a playbook naming it would be told to
/// wait for a release that will never contain it.
pub fn is_builtin(module: &str) -> bool {
    let ours = !module.contains('.')
        || module.starts_with("ansible.builtin.")
        || module.starts_with("ansible.legacy.");
    ours && BUILTIN_MODULES.contains(&short_name(module))
}

/// Whether this engine can run the module at all, natively on the agent or on the controller.
/// The two tables are all there is, so a playbook naming anything else can be refused while it
/// is being loaded, which is where the reference refuses a module it cannot resolve.
pub fn is_known(module: &str) -> bool {
    native(module).is_some() || local(module).is_some()
}

/// Where a builtin module runs, as the documentation page and the parity matrix say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `async_status`, `command`, `shell`, `raw`: the agent runs it, without Python.
    Agent,
    /// `assert`, `debug`, ...: the controller runs it.
    Controller,
    /// `import_*`, `include_*`, `meta`: the compiler answers it.
    Statement,
    /// One of [`ACTION_PLUGINS`].
    ActionPlugin,
    /// Python module with a native fast path in the agent ([`NATIVE_ENABLED`]).
    NativeOrPython,
    /// Python module on the warm path.
    Python,
}

/// Where a builtin module runs, or `None` when `module` is not a builtin.
pub fn builtin_route(module: &str) -> Option<Route> {
    if !is_builtin(module) {
        return None;
    }
    let m = short_name(module);
    Some(if native(m).is_some() {
        Route::Agent
    } else if local(m).is_some() {
        Route::Controller
    } else if import_module(m).is_some() || include_module(m).is_some() || m == "meta" {
        Route::Statement
    } else if ACTION_PLUGINS.iter().any(|(p, _)| *p == m) {
        Route::ActionPlugin
    } else if NATIVE_ENABLED.contains(&m) {
        Route::NativeOrPython
    } else {
        Route::Python
    })
}
