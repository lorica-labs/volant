// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a task's controller file is found, and how its bytes travel to the host.

use std::path::{Path, PathBuf};

use volant_protocol::encoding::b64_encode;
use volant_protocol::frame::MAX_FRAME_LEN;

use std::collections::BTreeSet;

use super::FileBlob;
use crate::compile::Origin;

/// The largest file one blob carries: one frame, which holds the blob's bytes alone, raw or
/// deflated to fewer.
pub(crate) const MAX_FILE_LEN: usize = MAX_FRAME_LEN;

/// Where the reference looks for a task's local file, in its order.
///
/// Measured on ansible-core 2.19.12 from its "Searched in" list: `<role>/<subdir>/`, `<role>/`,
/// `<role>/tasks/<subdir>/`, `<role>/tasks/`, `<playbook>/<subdir>/`, `<playbook>/`. The task's
/// own directory stands where `<role>/tasks/` is, so a task outside a role searches its own
/// directory and the playbook's, which for a task written in the playbook is the same one twice,
/// as the reference lists it. A name that already starts with `<subdir>/` is not looked for
/// under `<subdir>/` again, and an absolute one is taken as it is.
pub(crate) fn search_paths(
    origin: &Origin,
    playbook_dir: &Path,
    subdir: &str,
    name: &str,
) -> Vec<PathBuf> {
    let named = Path::new(name);
    if named.is_absolute() {
        return vec![named.to_path_buf()];
    }
    let under = name.split('/').next() != Some(subdir);
    let mut out = Vec::new();
    for base in origin
        .role_dir
        .iter()
        .map(PathBuf::as_path)
        .chain([origin.file_dir.as_path(), playbook_dir])
    {
        if under {
            out.push(base.join(subdir).join(name));
        }
        out.push(base.join(name));
    }
    out
}

/// The reference's refusal when none of the [`search_paths`] is there.
pub(crate) fn not_found(name: &str, searched: &[PathBuf]) -> String {
    let searched: Vec<String> = searched.iter().map(|p| p.display().to_string()).collect();
    format!(
        "Could not find or access '{name}'\nSearched in:\n\t{} on the Ansible Controller.",
        searched.join("\n\t")
    )
}

/// This engine's own refusal of a controller path a managed host chose, made before the path is
/// so much as looked up: a lookup that answers "not found" already tells a host whether a
/// controller path exists.
///
/// Stricter than the reference, on purpose. Measured on ansible-core 2.19.12, the reference
/// sends whatever controller file a registered value names, `~/.ssh` included, so a host that
/// controls a command's output can have any file the operator can read sent to it.
pub(crate) fn refuse_host_named(untrusted: &BTreeSet<String>, arg: &str) -> Result<(), String> {
    if untrusted.contains(arg) {
        return Err(format!(
            "the '{arg}' of this task was named by a managed host, and a controller file a host chose is never sent"
        ));
    }
    Ok(())
}

/// Refuses `len` bytes that could not fit in one frame, naming both sizes.
pub(crate) fn fits(name: &str, len: usize) -> Result<(), String> {
    if len > MAX_FILE_LEN {
        return Err(format!(
            "{name} is {len} bytes; one frame carries at most {MAX_FILE_LEN}"
        ));
    }
    Ok(())
}

/// A file's bytes as a blob, refused when they could not fit in one frame.
///
/// Named by the blake3 of the bytes, as the union is: the agent refuses a blob whose bytes hash
/// to anything else.
pub(crate) fn blob_of(name: &str, bytes: &[u8]) -> Result<FileBlob, String> {
    fits(name, bytes.len())?;
    Ok(FileBlob {
        hash: blake3::hash(bytes).to_hex().to_string(),
        b64: b64_encode(bytes),
    })
}

#[cfg(test)]
mod tests;
