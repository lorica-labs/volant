// SPDX-License-Identifier: GPL-3.0-or-later
//! The options `-o` and `-t` of `ansible`: where the tree is written and which notices they print.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::CommandFactory;

use super::args::AdhocArgs;

/// The directory of `-t` as the reference's `unfrackpath` leaves it: `~` expanded, made absolute
/// and folded. `Err` names what this release does not expand.
pub(super) fn tree_dir(dir: &Path) -> Result<PathBuf, String> {
    let text = dir.to_string_lossy();
    if text.contains('$') {
        return Err("a '$' in the --tree directory".into());
    }
    let expanded = match text.strip_prefix('~') {
        None => dir.to_path_buf(),
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .ok_or("a '~' in the --tree directory without a HOME")?;
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        Some(_) => return Err("a '~user' in the --tree directory".into()),
    };
    let absolute =
        std::path::absolute(&expanded).map_err(|e| format!("the --tree directory ({e})"))?;
    Ok(crate::roles::folded(&absolute))
}

/// The deprecated options that were given, each as the reference words its notice, in the order
/// of the command line and spelled as the line spelled them: argparse names the option string it
/// matched, `-o` and `-t` too inside a group such as `-ot`.
pub(super) fn deprecated_options(args: &AdhocArgs, argv: &[OsString]) -> Vec<String> {
    let command = AdhocArgs::command();
    let takes_value = |c: char| {
        command
            .get_arguments()
            .any(|a| a.get_short() == Some(c) && a.get_action().takes_values())
    };
    let mut found: Vec<&str> = Vec::new();
    for arg in argv.iter().filter_map(|arg| arg.to_str()) {
        if arg == "--" {
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long.split('=').next() {
                Some("one-line") => found.push("--one-line"),
                Some("tree") => found.push("--tree"),
                _ => {}
            }
        } else if let Some(group) = arg.strip_prefix('-') {
            for c in group.chars() {
                match c {
                    'o' => found.push("-o"),
                    't' => {
                        found.push("-t");
                        break;
                    }
                    c if takes_value(c) => break,
                    _ => {}
                }
            }
        }
    }
    let oneline = |s: &str| matches!(s, "-o" | "--one-line");
    let mut shown: Vec<&str> = Vec::new();
    for spelling in found {
        let given = if oneline(spelling) {
            args.one_line
        } else {
            args.tree.is_some()
        };
        if given && !shown.contains(&spelling) {
            shown.push(spelling);
        }
    }
    // The options clap read and the scan did not see: still deprecated.
    if args.one_line && !shown.iter().any(|s| oneline(s)) {
        shown.push("-o");
    }
    if args.tree.is_some() && shown.iter().all(|s| oneline(s)) {
        shown.push("-t");
    }
    shown
        .into_iter()
        .map(|spelling| {
            let callback = if oneline(spelling) { "oneline" } else { "tree" };
            format!(
                "The '{spelling}' argument is deprecated. This feature will be removed from ansible-core version 2.23. Use callback configuration to enable the {callback} callback instead."
            )
        })
        .collect()
}
