// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;

use sha2::{Digest as _, Sha256};

use super::Failure;
use super::artifact::Tarball;

/// A tarball of `files` (name, mode, content), with the manifest and the `FILES.json` that list
/// them under `extra_listed` too.
fn tarball(
    dir: &Path,
    files: &[(&str, u32, &[u8])],
    extra_listed: &[&str],
    links: &[(&str, &str)],
) -> String {
    let mut listed = vec![
        serde_json::json!({"name": ".", "ftype": "dir", "chksum_type": null, "chksum_sha256": null, "format": 1}),
    ];
    for (name, _, data) in files {
        listed.push(serde_json::json!({"name": name, "ftype": "file", "chksum_type": "sha256", "chksum_sha256": format!("{:x}", Sha256::digest(data)), "format": 1}));
    }
    for name in extra_listed {
        listed.push(serde_json::json!({"name": name, "ftype": "file", "chksum_type": "sha256", "chksum_sha256": "", "format": 1}));
    }
    for (name, _) in links {
        listed.push(serde_json::json!({"name": name, "ftype": "file", "chksum_type": "sha256", "chksum_sha256": "", "format": 1}));
    }
    let files_json =
        serde_json::to_vec(&serde_json::json!({"files": listed, "format": 1})).expect("json");
    let manifest = serde_json::to_vec(&serde_json::json!({"collection_info": {"namespace": "acme", "name": "demo", "version": "1.0.0", "dependencies": {}}, "format": 1})).expect("json");
    let path = dir.join("acme-demo-1.0.0.tar.gz");
    let gz = flate2::write::GzEncoder::new(
        std::fs::File::create(&path).expect("created"),
        flate2::Compression::default(),
    );
    let mut builder = tar::Builder::new(gz);
    let mut add = |name: &str, mode: u32, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        // Written by hand: the builder refuses the `..` a hostile tarball carries.
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        builder.append(&header, data).expect("appended");
    };
    add("MANIFEST.json", 0o644, &manifest);
    add("FILES.json", 0o644, &files_json);
    for (name, mode, data) in files {
        add(name, *mode, data);
    }
    for (name, target) in links {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        builder
            .append_link(&mut header, name, target)
            .expect("linked");
    }
    builder
        .into_inner()
        .expect("finished")
        .finish()
        .expect("flushed");
    path.display().to_string()
}

#[test]
fn the_manifest_names_the_collection() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = tarball(dir.path(), &[("README.md", 0o644, b"x\n")], &[], &[]);
    let (_, meta) = Tarball::open(&path).expect("opens");
    assert_eq!(
        (meta.fqcn().as_str(), meta.version.as_str()),
        ("acme.demo", "1.0.0")
    );
}

#[cfg(unix)]
#[test]
fn an_executable_file_keeps_its_x_bit_and_the_rest_is_0644() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("a directory");
    let path = tarball(
        dir.path(),
        &[
            ("README.md", 0o600, b"x\n"),
            ("plugins/modules/x.py", 0o700, b"print(1)\n"),
        ],
        &[],
        &[],
    );
    let (tar, _) = Tarball::open(&path).expect("opens");
    let dest = dir.path().join("cols/ansible_collections/acme/demo");
    tar.install(&dest).expect("installs");
    let mode = |p: &str| {
        std::fs::metadata(dest.join(p))
            .expect("there")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("README.md"), 0o644);
    assert_eq!(mode("plugins/modules/x.py"), 0o755);
    assert_eq!(mode("MANIFEST.json"), 0o644);
}

#[test]
fn a_path_that_leaves_the_collection_is_refused_and_nothing_is_left() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = tarball(dir.path(), &[("../escape.txt", 0o644, b"x\n")], &[], &[]);
    let (tar, _) = Tarball::open(&path).expect("opens");
    let dest = dir.path().join("cols/ansible_collections/acme/demo");
    assert_eq!(
        tar.install(&dest),
        Err(Failure::error(
            "Cannot extract tar entry '../escape.txt' as it will be placed outside the collection directory"
        ))
    );
    assert!(!dest.exists());
    assert!(!dest.parent().expect("namespace").exists());
    assert!(
        !dir.path()
            .join("cols/ansible_collections/acme/escape.txt")
            .exists()
    );
}

#[test]
fn a_link_that_leaves_the_collection_is_refused() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = tarball(dir.path(), &[], &[], &[("out", "../../secret")]);
    let (tar, _) = Tarball::open(&path).expect("opens");
    let dest = dir.path().join("cols/ansible_collections/acme/demo");
    assert_eq!(
        tar.install(&dest),
        Err(Failure::error(
            "Cannot extract symlink 'out' in collection: path points to location outside of collection 'b'../../secret''"
        ))
    );
    assert!(!dest.exists());
}

#[test]
fn a_listed_file_the_tarball_lacks_is_an_error() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = tarball(dir.path(), &[], &["README.md"], &[]);
    let (tar, _) = Tarball::open(&path).expect("opens");
    let dest = dir.path().join("cols/ansible_collections/acme/demo");
    assert_eq!(
        tar.install(&dest),
        Err(Failure::error(format!(
            "Collection tar at '{path}' does not contain the expected file 'README.md'."
        )))
    );
}

#[test]
fn a_file_that_is_not_a_tarball_is_refused() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("x.tar.gz");
    std::fs::write(&path, b"not a tarball").expect("written");
    let given = path.display().to_string();
    assert_eq!(
        Tarball::open(&given).err(),
        Some(Failure::error(format!(
            "Collection artifact at '{given}' is not a valid tar file."
        )))
    );
}
