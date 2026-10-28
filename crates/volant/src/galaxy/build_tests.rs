// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;

use super::artifact::Tarball;
use super::build::Source;

fn tree(dir: &Path, galaxy: &str) {
    std::fs::create_dir_all(dir.join("plugins/modules")).expect("made");
    std::fs::create_dir_all(dir.join(".git")).expect("made");
    std::fs::create_dir_all(dir.join("tests/output")).expect("made");
    std::fs::write(dir.join("galaxy.yml"), galaxy).expect("written");
    std::fs::write(dir.join("README.md"), "# demo — é\n").expect("written");
    std::fs::write(dir.join("plugins/modules/x.py"), "print(1)\n").expect("written");
    std::fs::write(dir.join("plugins/modules/x.pyc"), "compiled").expect("written");
    std::fs::write(dir.join(".git/HEAD"), "ref").expect("written");
    std::fs::write(dir.join("tests/output/log"), "out").expect("written");
    std::fs::write(dir.join("notes.txt"), "kept out").expect("written");
}

const GALAXY: &str = "namespace: acme\nname: demo\nversion: 1.0.0\nreadme: README.md\nauthors: [Zoë]\nbuild_ignore: ['*.txt']\n";

#[test]
fn the_same_tree_builds_to_the_same_bytes() {
    let dir = tempfile::tempdir().expect("a directory");
    let src = dir.path().join("src");
    tree(&src, GALAXY);
    let source = Source::read(&src).expect("a source");
    let first = source.build(dir.path(), false, 0).expect("built");
    let bytes = std::fs::read(&first).expect("read");
    let again = source.build(dir.path(), true, 0).expect("built again");
    assert_eq!(std::fs::read(again).expect("read"), bytes);
}

/// The ignore list of the reference, `build_ignore`, and the manifests themselves are left out;
/// the built tarball installs.
#[test]
fn a_build_leaves_out_what_the_reference_leaves_out() {
    let dir = tempfile::tempdir().expect("a directory");
    let src = dir.path().join("src");
    tree(&src, GALAXY);
    let built = Source::read(&src)
        .expect("a source")
        .build(dir.path(), false, 0)
        .expect("built");
    let (tarball, meta) = Tarball::open(&built.display().to_string()).expect("opens");
    assert_eq!(
        (meta.fqcn().as_str(), meta.version.as_str()),
        ("acme.demo", "1.0.0")
    );
    let dest = dir.path().join("cols/ansible_collections/acme/demo");
    tarball.install(&dest).expect("installs");
    let files: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dest.join("FILES.json")).expect("read"))
            .expect("json");
    let names: Vec<&str> = files["files"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|e| e["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        [
            ".",
            "README.md",
            "plugins",
            "plugins/modules",
            "plugins/modules/x.py",
            "tests"
        ]
    );
}

/// `json.dumps` escapes every character outside ASCII and indents by one space.
#[test]
fn the_manifest_is_written_as_python_writes_it() {
    let dir = tempfile::tempdir().expect("a directory");
    let src = dir.path().join("src");
    tree(&src, GALAXY);
    let dest = dir.path().join("cols/ansible_collections/acme/demo");
    Source::read(&src)
        .expect("a source")
        .install(&dest, 0)
        .expect("installed");
    let manifest = std::fs::read_to_string(dest.join("MANIFEST.json")).expect("read");
    assert!(
        manifest.starts_with("{\n \"collection_info\": {\n  \"namespace\": \"acme\",\n"),
        "{manifest}"
    );
    assert!(manifest.contains("\"Zo\\u00eb\""), "{manifest}");
    assert!(manifest.contains("\"license_file\": null"), "{manifest}");
    assert!(manifest.contains("\"tags\": []"), "{manifest}");
    assert!(!dest.join("notes.txt").exists());
}

#[test]
fn a_galaxy_yml_without_a_required_key_is_refused() {
    let dir = tempfile::tempdir().expect("a directory");
    tree(dir.path(), "namespace: acme\nname: demo\nversion: 1.0.0\n");
    let failure = Source::read(dir.path()).err().expect("refused");
    assert!(
        failure
            .message
            .ends_with("is missing the following mandatory keys: authors, readme"),
        "{}",
        failure.message
    );
}

#[cfg(unix)]
#[test]
fn an_executable_keeps_its_bit_and_a_link_inside_stays_a_link() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("a directory");
    let src = dir.path().join("src");
    tree(&src, GALAXY);
    let script = src.join("plugins/modules/x.py");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    std::os::unix::fs::symlink("modules/x.py", src.join("plugins/y.py")).expect("linked");
    let built = Source::read(&src)
        .expect("a source")
        .build(dir.path(), false, 0)
        .expect("built");
    let (tarball, _) = Tarball::open(&built.display().to_string()).expect("opens");
    let dest = dir.path().join("cols/ansible_collections/acme/demo");
    tarball.install(&dest).expect("installs");
    let mode = std::fs::metadata(dest.join("plugins/modules/x.py"))
        .expect("there")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(
        std::fs::read_link(dest.join("plugins/y.py")).expect("a link"),
        Path::new("modules/x.py")
    );
}
