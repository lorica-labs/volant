// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;

use rstest::rstest;
use serde_json::json;

use super::Failure;
use super::requirements::{Collection, Kind, Requirements, Role, collection, read, role};

fn written(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("requirements.yml");
    std::fs::write(&path, text).expect("written");
    (dir, path)
}

fn galaxy(name: &str, version: &str) -> Collection {
    Collection {
        name: Some(name.into()),
        version: version.into(),
        source: None,
        kind: Kind::Galaxy,
        signatures: Vec::new(),
    }
}

fn galaxy_role(name: &str, version: &str) -> Role {
    Role {
        name: Some(name.into()),
        src: None,
        scm: None,
        version: Some(version.into()),
    }
}

const NOT_FQCN: &str = "Neither the collection requirement entry key 'name', nor 'source' point to a concrete resolvable collection artifact. Also 'name' is not an FQCN. A valid collection name must be in the format <namespace>.<collection>. Please make sure that the namespace and the collection name contain characters from [a-zA-Z0-9_] only.";

/// Measured on ansible-core 2.19.12, `collection install -r` and `role install -r`.
#[rstest]
#[case::empty("", false, "No requirements found in file '{path}'")]
#[case::bare_list_for_collections(
    "- name: demo.web\n",
    false,
    "Expecting requirements file to be a dict with the key 'collections' that contains a list of collections to install"
)]
#[case::other_key(
    "foo: 1\n",
    true,
    "Expecting only 'roles' and/or 'collections' as base keys in the requirements file. Found: foo"
)]
#[case::scalar(
    "42\n",
    true,
    "Expecting requirements yaml to be a list or dictionary but got int"
)]
#[case::role_without_name(
    "roles:\n  - version: 1.0.0\n",
    true,
    "Must specify name or src for role"
)]
#[case::missing_include(
    "roles:\n  - include: nonexistent-inc.yml\n",
    true,
    "Failed to find include requirements file 'nonexistent-inc.yml' in '{path}'"
)]
#[case::not_fqcn(
    "collections:\n  - name: notacollection\n",
    false,
    "{not_fqcn}\n\nCould not find notacollection."
)]
#[case::name_with_version(
    "collections:\n  - demo.base:1.0.0\n",
    false,
    "{not_fqcn}\n\nCould not find demo.base:1.0.0."
)]
#[case::no_name("collections:\n  - version: 1.0.0\n", false, "{not_fqcn}")]
#[case::bad_type(
    "collections:\n  - name: a.b\n    type: svn\n",
    false,
    "The collection requirement entry key 'type' must be one of file, galaxy, git, dir, subdirs, or url."
)]
fn a_requirements_file_is_refused(
    #[case] text: &str,
    #[case] old_format: bool,
    #[case] want: &str,
) {
    let (_dir, path) = written(text);
    let want = want
        .replace("{path}", &path.display().to_string())
        .replace("{not_fqcn}", NOT_FQCN);
    assert_eq!(read(&path, old_format), Err(Failure::error(want)));
}

#[test]
fn both_kinds_come_from_one_file() {
    let (_dir, path) = written(
        "collections:\n  - demo.base\n  - name: demo.app\n    version: '>=1.0.0,<2.0.0'\n    source: https://galaxy.example/\n    signatures: [file:///s.asc]\nroles:\n  - demo.web,1.0.0\n  - name: web2\n    src: https://github.com/demo/ansible-role-web\n    version: 1.0\n",
    );
    let mut app = galaxy("demo.app", ">=1.0.0,<2.0.0");
    app.source = Some("https://galaxy.example/".into());
    app.signatures = vec!["file:///s.asc".into()];
    assert_eq!(
        read(&path, true),
        Ok(Requirements {
            collections: vec![galaxy("demo.base", "*"), app],
            roles: vec![
                Role {
                    name: Some("demo.web".into()),
                    src: Some("demo.web".into()),
                    scm: None,
                    version: Some("1.0.0".into()),
                },
                Role {
                    name: Some("web2".into()),
                    src: Some("https://github.com/demo/ansible-role-web".into()),
                    scm: Some("git".into()),
                    version: Some("1.0".into()),
                },
            ],
        })
    );
}

#[test]
fn a_file_without_entries_for_the_command_is_empty() {
    let (_dir, path) = written("roles:\n  - name: demo.web\n");
    assert!(read(&path, false).expect("parses").collections.is_empty());
    let (_dir, path) = written("collections:\n  - demo.base\n");
    assert!(read(&path, true).expect("parses").roles.is_empty());
}

/// An `include` brings the roles of its file in place of the entry.
#[test]
fn an_include_brings_its_roles_in_place() {
    let dir = tempfile::tempdir().expect("a directory");
    let included = dir.path().join("more.yml");
    std::fs::write(&included, "- name: demo.web\n  version: 1.0.0\n").expect("written");
    let path = dir.path().join("requirements.yml");
    std::fs::write(
        &path,
        format!(
            "roles:\n  - include: '{}'\n  - name: demo.other\n",
            included.display()
        ),
    )
    .expect("written");
    assert_eq!(
        read(&path, true).expect("parses").roles,
        [
            galaxy_role("demo.web", "1.0.0"),
            galaxy_role("demo.other", "")
        ]
    );
}

#[rstest]
#[case::name("geerlingguy.ntp", "geerlingguy.ntp", "geerlingguy.ntp", None, None)]
#[case::version(
    "geerlingguy.ntp,4.0.1",
    "geerlingguy.ntp",
    "geerlingguy.ntp",
    None,
    Some("4.0.1")
)]
#[case::renamed(
    "geerlingguy.ntp,4.0.1,ntp",
    "ntp",
    "geerlingguy.ntp",
    None,
    Some("4.0.1")
)]
#[case::git(
    "git+https://github.com/demo/ansible-role-web.git",
    "ansible-role-web",
    "https://github.com/demo/ansible-role-web.git",
    Some("git"),
    None
)]
#[case::tarball(
    "https://example.com/web.tar.gz",
    "web",
    "https://example.com/web.tar.gz",
    None,
    None
)]
fn a_role_line(
    #[case] line: &str,
    #[case] name: &str,
    #[case] src: &str,
    #[case] scm: Option<&str>,
    #[case] version: Option<&str>,
) {
    assert_eq!(
        role(&json!(line)),
        Ok(Role {
            name: Some(name.into()),
            src: Some(src.into()),
            scm: scm.map(Into::into),
            version: version.map(Into::into),
        })
    );
}

#[test]
fn a_role_line_with_too_many_commas_is_refused() {
    assert_eq!(
        role(&json!("a,b,c,d")),
        Err(Failure::error(
            "Invalid role line (a,b,c,d). Proper format is 'role_name[,version[,name]]'"
        ))
    );
}

#[rstest]
#[case::git("git+https://example.com/c.git", Kind::Git)]
#[case::url("https://example.com/c.tar.gz", Kind::Url)]
fn a_collection_pointer_gets_its_type(#[case] name: &str, #[case] kind: Kind) {
    let parsed = collection(&json!(name)).expect("parses");
    assert_eq!(parsed.kind, kind);
    assert_eq!(parsed.name, None);
    assert_eq!(parsed.source.as_deref(), Some(name));
}

#[test]
fn a_collection_directory_and_a_namespace_directory() {
    let dir = tempfile::tempdir().expect("a directory");
    let one = dir.path().join("ns").join("one");
    std::fs::create_dir_all(&one).expect("made");
    std::fs::write(one.join("galaxy.yml"), "namespace: ns\n").expect("written");
    let one = one.display().to_string();
    assert_eq!(collection(&json!(one)).expect("parses").kind, Kind::Dir);
    let ns = dir.path().join("ns").display().to_string();
    assert_eq!(collection(&json!(ns)).expect("parses").kind, Kind::Subdirs);
}

#[test]
fn a_number_entry_is_an_unexpected_exception() {
    assert_eq!(
        collection(&json!(42)),
        Err(Failure::unexpected("unsupported type <class 'int'>"))
    );
}

#[test]
fn the_fixture_files_parse() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let roles = read(&fixtures.join("proof-roles/requirements.yml"), true).expect("parses");
    assert_eq!(roles.roles.len(), 4);
    assert_eq!(roles.roles[0], galaxy_role("geerlingguy.security", "3.0.2"));
    let k3s = read(&fixtures.join("proof-k3s/requirements.yml"), false).expect("parses");
    assert_eq!(k3s.collections.len(), 4);
    assert_eq!(k3s.collections[0], galaxy("ansible.posix", "2.2.2"));
}
