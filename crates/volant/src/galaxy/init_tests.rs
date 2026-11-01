// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Map, Value};

use super::init::{Init, collection};

fn init<'a>(name: &'a str, path: &'a str, skeleton: Option<&'a str>) -> Init<'a> {
    let mut extra_vars = Map::new();
    extra_vars.insert("min_ansible_version".into(), Value::String("2.17".into()));
    Init {
        name,
        init_path: path,
        force: false,
        skeleton,
        ignore: &[],
        extra_vars,
    }
}

/// ansible-core's default skeleton, as ansible-galaxy 2.19.12 writes it for `ns_x.name_y`.
#[test]
fn the_default_skeleton_is_ansible_cores() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().to_str().expect("a UTF-8 path");
    assert_eq!(collection(&init("ns_x.name_y", path, None)).ok(), Some(0));
    let root = dir.path().join("ns_x/name_y");
    let read = |p: &str| std::fs::read_to_string(root.join(p)).expect("written");
    assert_eq!(
        read("README.md"),
        "# Ansible Collection - ns_x.name_y\n\nDocumentation for the collection.\n"
    );
    assert!(read("galaxy.yml").contains("\nnamespace: ns_x\n"));
    assert!(read("galaxy.yml").contains("\nname: name_y\n"));
    assert!(read("plugins/README.md").ends_with(
        "[Working With Plugins](https://docs.ansible.com/ansible-core/2.19/plugins/plugins.html).\n"
    ));
    assert!(read("meta/runtime.yml").starts_with("#SPDX-License-Identifier: MIT-0\n---\n"));
    for empty in ["docs", "roles"] {
        assert_eq!(
            std::fs::read_dir(root.join(empty)).expect("made").count(),
            0
        );
    }
    let again = collection(&init("ns_x.name_y", path, None)).expect_err("it exists");
    assert!(
        again
            .message
            .contains("already exists. You can use --force")
    );
}

/// A skeleton of the operator's: its `.j2` files rendered with `--extra-vars`, the rest copied.
#[test]
fn a_skeleton_directory_is_rendered_and_copied() {
    let dir = tempfile::tempdir().expect("a directory");
    let skeleton = dir.path().join("skel");
    std::fs::create_dir_all(skeleton.join("plugins/.git")).expect("made");
    std::fs::write(
        skeleton.join("README.j2"),
        "Requires ansible-core >={{ min_ansible_version }}\n",
    )
    .expect("written");
    std::fs::write(skeleton.join("plugins/x.py"), "x\n").expect("written");
    let path = dir.path().to_str().expect("a UTF-8 path");
    let ignore = ["^.git$".to_string()];
    let given = Init {
        ignore: &ignore,
        ..init("ns.coll", path, skeleton.to_str())
    };
    assert_eq!(collection(&given).ok(), Some(0));
    let root = dir.path().join("ns/coll");
    assert_eq!(
        std::fs::read_to_string(root.join("README")).expect("rendered"),
        "Requires ansible-core >=2.17\n"
    );
    assert!(root.join("plugins/x.py").is_file());
    assert!(!root.join("plugins/.git").exists());
}
