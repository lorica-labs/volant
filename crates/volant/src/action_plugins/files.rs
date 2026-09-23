// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a task's controller file is found, and how its bytes travel to the host.

use std::path::{Path, PathBuf};

use volant_protocol::encoding::b64_encode;
use volant_protocol::frame::MAX_FRAME_LEN;

use super::FileBlob;
use crate::compile::Origin;

/// The largest file one blob carries: what fits in a frame once in base64, with room left for
/// the message around it.
pub(crate) const MAX_FILE_LEN: usize = (MAX_FRAME_LEN - 4096) / 4 * 3;

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

/// A file's bytes as a blob, refused when they could not fit in one frame.
///
/// Named by the blake3 of the bytes, as the union is: the agent refuses a blob whose bytes hash
/// to anything else.
pub(crate) fn blob_of(name: &str, bytes: &[u8]) -> Result<FileBlob, String> {
    if bytes.len() > MAX_FILE_LEN {
        return Err(format!(
            "{name} is {} bytes; one frame carries at most {MAX_FILE_LEN}",
            bytes.len()
        ));
    }
    Ok(FileBlob {
        hash: blake3::hash(bytes).to_hex().to_string(),
        b64: b64_encode(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order the reference searches in, with a role and without one.
    ///
    /// What would make this red: the playbook's directory searched before the role's, which
    /// copies a playbook's file of the same name where the role meant its own, or `files/`
    /// looked for under `files/` again.
    #[test]
    fn a_file_is_searched_where_the_reference_searches() {
        let role = Origin {
            file_dir: PathBuf::from("/pb/roles/r/tasks"),
            role_dir: Some(PathBuf::from("/pb/roles/r")),
            depth: 0,
        };
        let pb = Path::new("/pb");
        assert_eq!(
            search_paths(&role, pb, "files", "a.txt"),
            [
                "/pb/roles/r/files/a.txt",
                "/pb/roles/r/a.txt",
                "/pb/roles/r/tasks/files/a.txt",
                "/pb/roles/r/tasks/a.txt",
                "/pb/files/a.txt",
                "/pb/a.txt",
            ]
            .map(PathBuf::from)
        );
        let play = Origin {
            file_dir: PathBuf::from("/pb"),
            ..Origin::default()
        };
        assert_eq!(
            search_paths(&play, pb, "files", "files/a.txt"),
            ["/pb/files/a.txt", "/pb/files/a.txt"].map(PathBuf::from)
        );
        assert_eq!(
            search_paths(&play, pb, "files", "/etc/a.txt"),
            [PathBuf::from("/etc/a.txt")]
        );
        assert_eq!(
            not_found("a.txt", &search_paths(&play, pb, "files", "a.txt")),
            "Could not find or access 'a.txt'\nSearched in:\n\t/pb/files/a.txt\n\t/pb/a.txt\n\t/pb/files/a.txt\n\t/pb/a.txt on the Ansible Controller."
        );
    }

    /// A file too big for one frame is refused before anything is encoded, naming both sizes.
    ///
    /// What would make this red: the limit dropped, which encodes a 60 MiB file into a frame the
    /// agent refuses to read and ends the link, or a message without the two numbers.
    #[test]
    fn a_file_bigger_than_a_frame_is_refused_naming_both_sizes() {
        let big = vec![0u8; 60 * 1024 * 1024];
        let err = blob_of("big.bin", &big).expect_err("60 MiB does not fit");
        assert_eq!(
            err,
            format!("big.bin is 62914560 bytes; one frame carries at most {MAX_FILE_LEN}")
        );
        let blob = blob_of("x", b"x").expect("one byte fits");
        assert_eq!(blob.b64, "eA==");
        assert_eq!(blob.hash, blake3::hash(b"x").to_hex().to_string());
    }
}
