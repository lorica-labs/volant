// SPDX-License-Identifier: GPL-3.0-or-later
//! The directories of `-M`/`--module-path`.

static MODULE_PATH: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Sets the directories of `-M`/`--module-path`, which hold modules directly and lead the
/// configured module path, so a playbook's or role's `library/` is still searched before them.
pub fn set_module_path(dirs: Vec<std::path::PathBuf>) {
    *MODULE_PATH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = dirs;
}

/// The directories [`set_module_path`] set.
pub fn module_path() -> Vec<std::path::PathBuf> {
    MODULE_PATH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
