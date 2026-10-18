// SPDX-License-Identifier: GPL-3.0-or-later
//! What `apt` accepts: its `argument_spec`, and how the native takes each argument.

use serde_json::Value;

use super::super::common::ArgSpec;
/// `apt`'s `argument_spec` in ansible-core 2.19.12, with its defaults and aliases.
pub(super) const SPEC: &[ArgSpec] = &[
    ArgSpec {
        name: "state",
        aliases: &[],
        default: || "present".into(),
    },
    ArgSpec {
        name: "update_cache",
        aliases: &["update-cache"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "update_cache_retries",
        aliases: &[],
        default: || 5.into(),
    },
    ArgSpec {
        name: "update_cache_retry_max_delay",
        aliases: &[],
        default: || 12.into(),
    },
    ArgSpec {
        name: "cache_valid_time",
        aliases: &[],
        default: || 0.into(),
    },
    ArgSpec {
        name: "purge",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "package",
        aliases: &["pkg", "name"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "deb",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "default_release",
        aliases: &["default-release"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "install_recommends",
        aliases: &["install-recommends"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "force",
        aliases: &[],
        default: || false.into(),
    },
    // `"no"` in the spec; the module sets it to `None` before it answers, and the
    // `invocation` shows that.
    ArgSpec {
        name: "upgrade",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "dpkg_options",
        aliases: &[],
        default: || "force-confdef,force-confold".into(),
    },
    ArgSpec {
        name: "autoremove",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "autoclean",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "fail_on_autoremove",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "policy_rc_d",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "only_upgrade",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "force_apt_get",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "clean",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "allow_unauthenticated",
        aliases: &["allow-unauthenticated"],
        default: || false.into(),
    },
    ArgSpec {
        name: "allow_downgrade",
        aliases: &["allow-downgrade", "allow_downgrades", "allow-downgrades"],
        default: || false.into(),
    },
    ArgSpec {
        name: "allow_change_held_packages",
        aliases: &[],
        default: || false.into(),
    },
    ArgSpec {
        name: "lock_timeout",
        aliases: &[],
        default: || 60.into(),
    },
    ArgSpec {
        name: "auto_install_module_deps",
        aliases: &[],
        default: || true.into(),
    },
];

/// How the native takes each argument it accepts; an argument missing here hands back when
/// given at all.
#[derive(Clone, Copy)]
pub(super) enum Take {
    /// `type='bool'`, any value: it only shapes a command the fast path never runs.
    Bool,
    /// `type='bool'` with no default: `null` stays `null`.
    OptBool,
    /// `type='bool'` that must read false: true asks for work the fast path does not do.
    False,
    /// `type='int'`, any value.
    Int,
    /// `type='int'` with no default.
    OptInt,
    /// `type='str'`, any value.
    Str,
}

pub(super) const TAKEN: &[(&str, Take)] = &[
    ("update_cache", Take::OptBool),
    ("update_cache_retries", Take::Int),
    ("update_cache_retry_max_delay", Take::Int),
    ("purge", Take::False),
    ("install_recommends", Take::OptBool),
    ("force", Take::Bool),
    ("dpkg_options", Take::Str),
    ("autoremove", Take::False),
    ("autoclean", Take::False),
    ("fail_on_autoremove", Take::Bool),
    ("policy_rc_d", Take::OptInt),
    ("only_upgrade", Take::False),
    ("force_apt_get", Take::Bool),
    ("clean", Take::False),
    ("allow_unauthenticated", Take::Bool),
    ("allow_downgrade", Take::Bool),
    ("allow_change_held_packages", Take::Bool),
    ("lock_timeout", Take::Int),
    ("auto_install_module_deps", Take::Bool),
];
