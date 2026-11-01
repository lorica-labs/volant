// SPDX-License-Identifier: GPL-3.0-or-later
//! The names a variable should not take: `ansible/vars/reserved.py` (2.19.12), the templating
//! globals and every keyword of a play, a role, a block and a task.

use crate::playbook::Origin;

/// `_RESERVED_NAMES`, measured on ansible-core 2.19.12. `vars` is left out: the reference adds
/// it itself and never warns about it.
const RESERVED: &[&str] = &[
    "action",
    "always",
    "any_errors_fatal",
    "args",
    "async",
    "async_val",
    "become",
    "become_exe",
    "become_flags",
    "become_method",
    "become_user",
    "block",
    "changed_when",
    "check_mode",
    "collections",
    "connection",
    "cycler",
    "debugger",
    "delay",
    "delegate_facts",
    "delegate_to",
    "dict",
    "diff",
    "environment",
    "fact_path",
    "failed_when",
    "force_handlers",
    "gather_facts",
    "gather_timeout",
    "handlers",
    "hosts",
    "ignore_errors",
    "ignore_unreachable",
    "joiner",
    "lipsum",
    "local_action",
    "lookup",
    "loop",
    "loop_control",
    "loop_with",
    "max_fail_percentage",
    "module_defaults",
    "name",
    "namespace",
    "no_log",
    "notify",
    "now",
    "omit",
    "order",
    "poll",
    "port",
    "post_tasks",
    "pre_tasks",
    "q",
    "query",
    "range",
    "register",
    "remote_user",
    "rescue",
    "retries",
    "roles",
    "run_once",
    "serial",
    "strategy",
    "tags",
    "tasks",
    "throttle",
    "timeout",
    "undef",
    "until",
    "vars_files",
    "vars_prompt",
    "when",
    "with_",
];

/// Warns once per place when `name` is reserved, as `warn_if_reserved` does: measured,
/// `[WARNING]: Found variable using reserved name 'query'.` and the place the name was written.
pub(crate) fn warn_if_reserved(name: &str, origin: Option<&Origin>) {
    if !is_reserved(name) {
        return;
    }
    let place = origin
        .map(|o| format!("{}\n\n", crate::render::source_context(o)))
        .unwrap_or_default();
    crate::render::warn_once(&format!(
        "[WARNING]: Found variable using reserved name '{name}'.\n{place}"
    ));
}

/// Whether a variable named `name` draws the warning.
pub(crate) fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&name)
}

/// Where a top-level `name:` of a YAML vars file is written: the first line that starts with it.
pub(crate) fn top_level_key(path: &str, text: &str, name: &str) -> Option<Origin> {
    let line = text.lines().position(|l| {
        l.strip_prefix(name)
            .is_some_and(|rest| rest.trim_start().starts_with(':'))
    })?;
    Some(Origin {
        path: path.into(),
        line: line + 1,
        col: 1,
    })
}
