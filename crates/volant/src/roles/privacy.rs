// SPDX-License-Identifier: GPL-3.0-or-later
//! `private_role_vars`: whether a role that does not say `public` lends its variables.

/// `[defaults] private_role_vars`: 0 when the file does not say, 1 for false, 2 for true.
static PRIVATE_ROLE_VARS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Sets what `[defaults] private_role_vars` says.
pub fn set_private_role_vars(private: bool) {
    PRIVATE_ROLE_VARS.store(1 + u8::from(private), std::sync::atomic::Ordering::Relaxed);
}

/// `private_role_vars`, from `ANSIBLE_PRIVATE_ROLE_VARS` and then the configuration file, `None`
/// when neither says. Measured on ansible-core 2.19.12 with the `roles` target's `privacy.yml`:
/// set true, a role of `roles:` or of an `import_role` that does not say `public` keeps its
/// variables to itself; set false, even an `include_role` that does not say `public` lends them,
/// where with no setting it does not.
pub fn private_role_vars() -> Option<bool> {
    let from_env = std::env::var("ANSIBLE_PRIVATE_ROLE_VARS")
        .ok()
        .and_then(|text| match text.trim().to_ascii_lowercase().as_str() {
            "1" | "y" | "yes" | "t" | "true" | "on" => Some(true),
            "0" | "n" | "no" | "f" | "false" | "off" => Some(false),
            _ => None,
        });
    from_env.or(
        match PRIVATE_ROLE_VARS.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            stored => Some(stored == 2),
        },
    )
}
