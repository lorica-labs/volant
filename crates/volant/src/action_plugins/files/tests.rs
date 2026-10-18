// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// The order the reference searches in, with a role and without one.
#[test]
fn a_file_is_searched_where_the_reference_searches() {
    let role = Origin {
        file_dir: PathBuf::from("/pb/roles/r/tasks"),
        role_dir: Some(PathBuf::from("/pb/roles/r")),
        depth: 0,
        inherited: None,
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
#[test]
fn a_file_bigger_than_a_frame_is_refused_naming_both_sizes() {
    FRAME_LIMIT.set(1024);
    let big = vec![0u8; 1025];
    let err = blob_of("big.bin", &big).expect_err("1025 bytes do not fit");
    assert_eq!(err, "big.bin is 1025 bytes; one frame carries at most 1024");
    assert!(blob_of("fits.bin", &big[..1024]).is_ok());
    let blob = blob_of("x", b"x").expect("one byte fits");
    assert_eq!(blob.b64, "eA==");
    assert_eq!(blob.hash, blake3::hash(b"x").to_hex().to_string());
}
