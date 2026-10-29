// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant compat install`: the `volant.compat` collection the binary carries, installed where
//! `volant galaxy collection install` puts a collection, through the same directory install.

use clap::{Parser as _, Subcommand};

/// The collection's files, relative to its root. `collections/ansible_collections/volant/compat`
/// in the repository holds the same ones, which a test checks; the copy lives in the crate so the
/// published crate carries it.
pub(crate) const FILES: [(&str, &str); 4] = [
    ("README.md", include_str!("../../compat/README.md")),
    ("galaxy.yml", include_str!("../../compat/galaxy.yml")),
    (
        "meta/runtime.yml",
        include_str!("../../compat/meta/runtime.yml"),
    ),
    (
        "plugins/callback/record.py",
        include_str!("../../compat/plugins/callback/record.py"),
    ),
];

#[derive(Subcommand, Debug)]
pub enum CompatCommand {
    /// Install the volant.compat collection this binary carries, with no download.
    Install {
        /// The collections directory, `ansible_collections` added unless it ends with it. The
        /// first collections path by default.
        #[arg(
            short = 'p',
            long = "collections-path",
            value_name = "COLLECTIONS_PATH"
        )]
        collections_path: Option<String>,
        /// Reinstall it even when the same version is installed.
        #[arg(short = 'f', long)]
        force: bool,
    },
}

/// Runs `command` and returns the exit code.
#[must_use]
pub fn compat(command: CompatCommand) -> i32 {
    let CompatCommand::Install {
        collections_path,
        force,
    } = command;
    match install(collections_path.as_deref(), force) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("[ERROR]: {message}");
            1
        }
    }
}

fn install(collections_path: Option<&str>, force: bool) -> Result<i32, String> {
    let holder = tempfile::tempdir().map_err(|e| e.to_string())?;
    let source = holder.path().join("compat");
    write(&source).map_err(|e| e.to_string())?;
    let mut argv = vec![
        "galaxy".to_owned(),
        "collection".to_owned(),
        "install".to_owned(),
        source.to_string_lossy().into_owned(),
    ];
    if let Some(path) = collections_path {
        argv.extend(["-p".to_owned(), path.to_owned()]);
    }
    if force {
        argv.push("--force".to_owned());
    }
    let parsed = super::GalaxyArgs::try_parse_from(argv).map_err(|e| e.to_string())?;
    Ok(super::galaxy(parsed))
}

/// Writes the collection under `dir`, its version that of this binary.
fn write(dir: &std::path::Path) -> std::io::Result<()> {
    for (name, text) in FILES {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = if name == "galaxy.yml" {
            with_version(text, env!("CARGO_PKG_VERSION"))
        } else {
            text.to_owned()
        };
        std::fs::write(path, text)?;
    }
    Ok(())
}

/// `galaxy.yml` with its `version:` line set to `version`.
pub(crate) fn with_version(galaxy: &str, version: &str) -> String {
    galaxy
        .split_inclusive('\n')
        .map(|line| {
            if line.starts_with("version:") {
                format!("version: {version}\n")
            } else {
                line.to_owned()
            }
        })
        .collect()
}
