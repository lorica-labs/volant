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

/// The names of the modules in the `library/` of each of `dirs` and in the [`module_path()`]
/// directories, as the module loader finds them: a file's name less its extension. A Windows
/// module (`.ps1`) is left out, as a POSIX host never runs one.
pub(crate) fn library_modules(dirs: &[std::path::PathBuf]) -> std::collections::BTreeSet<String> {
    dirs.iter()
        .map(|dir| dir.join("library"))
        .chain(module_path())
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_none_or(|ext| ext != "ps1"))
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
        .filter(|name| !name.starts_with(['.', '_']))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_library_lists_its_modules_by_name() {
        let dir = tempfile::tempdir().unwrap();
        for file in [
            "library/hello.py",
            "library/helloworld_linux_x86_64",
            "library/win.ps1",
            "library/__init__.py",
        ] {
            crate::testing::write(dir.path(), file, "");
        }
        let names = super::library_modules(&[dir.path().to_path_buf(), dir.path().join("none")]);
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            ["hello", "helloworld_linux_x86_64"]
        );
    }
}
