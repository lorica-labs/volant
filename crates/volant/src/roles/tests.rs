// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::testing::scratch;

fn tree() -> tempfile::TempDir {
    let tmp = scratch();
    let dir = tmp.path();
    std::fs::create_dir_all(dir.join("roles/base/defaults/main")).unwrap();
    std::fs::create_dir_all(dir.join("roles/base/vars")).unwrap();
    std::fs::create_dir_all(dir.join("roles/base/tasks")).unwrap();
    std::fs::create_dir_all(dir.join("roles/base/meta")).unwrap();
    std::fs::write(
        dir.join("roles/base/defaults/main/a.yml"),
        "x: a\ny: from-first\n",
    )
    .unwrap();
    std::fs::write(dir.join("roles/base/defaults/main/b.yml"), "x: b\n").unwrap();
    std::fs::write(dir.join("roles/base/vars/main.yml"), "v: role-var\n").unwrap();
    std::fs::write(
        dir.join("roles/base/tasks/main.yml"),
        "- name: base task\n  debug: msg=hi\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("roles/base/meta/main.yml"),
        "allow_duplicates: true\ndependencies:\n  - role: other\n    p: v\n",
    )
    .unwrap();
    tmp
}

/// The measured reading rules for a role's directories: a `defaults/main/` directory is merged
/// in name order, and `meta/main.yml` is read for both of the things it can say.
#[test]
fn a_defaults_directory_merges_its_files_in_name_order() {
    let tmp = tree();
    let dir = tmp.path().to_path_buf();
    let role = load(&dir.join("roles/base"), &RoleFrom::default()).unwrap();
    assert_eq!(role.defaults["x"], serde_json::json!("b"), "b.yml is later");
    assert_eq!(role.defaults["y"], serde_json::json!("from-first"));
    assert_eq!(role.vars["v"], serde_json::json!("role-var"));
    assert_eq!(role.tasks.len(), 1);
    assert!(role.allow_duplicates);
    assert_eq!(role.dependencies.len(), 1);
    assert_eq!(role.dependencies[0].name, "other");
    assert_eq!(role.dependencies[0].params["p"], serde_json::json!("v"));
}

/// `defaults/main.yml` beside a `defaults/main/` directory wins outright and the directory is
/// not read, as in ansible-core 2.19.12.
#[test]
fn a_file_beats_a_directory_of_the_same_name() {
    let tmp = tree();
    let dir = tmp.path().to_path_buf();
    std::fs::write(dir.join("roles/base/defaults/main.yml"), "x: fromfile\n").unwrap();
    let role = load(&dir.join("roles/base"), &RoleFrom::default()).unwrap();
    assert_eq!(role.defaults["x"], serde_json::json!("fromfile"));
    assert!(
        !role.defaults.contains_key("y"),
        "the directory is not read at all"
    );
}

/// A file the entry asked for by name and that is not there stops the run in the reference's
/// own words; one it never asked for is simply absent.
#[test]
fn a_named_file_that_is_not_there_is_refused_and_a_missing_main_is_not() {
    let tmp = tree();
    let dir = tmp.path().to_path_buf();
    let from = RoleFrom {
        tasks: "nosuch".to_string(),
        ..RoleFrom::default()
    };
    let err = load(&dir.join("roles/base"), &from).unwrap_err();
    assert!(
        format!("{err:#}").contains("Could not find specified file in role: tasks/nosuch"),
        "{err:#}"
    );
    std::fs::create_dir_all(dir.join("roles/bare/defaults")).unwrap();
    std::fs::write(dir.join("roles/bare/defaults/main.yml"), "q: 1\n").unwrap();
    let role = load(&dir.join("roles/bare"), &RoleFrom::default()).unwrap();
    assert!(role.tasks.is_empty(), "a role may have no tasks at all");
    assert_eq!(role.defaults["q"], serde_json::json!(1));
}

/// The searched list, and the sentence a missing role is refused with, are the reference's own
/// - and the exit code is 1, not the 4 the rest of an unusable playbook gets.
#[test]
fn a_missing_role_names_every_path_and_exits_1() {
    let tmp = tree();
    let dir = tmp.path().to_path_buf();
    let config = Config {
        roles_path: vec![PathBuf::from("/opt/roles")],
        ..Config::default()
    };
    let search = RoleSearch::new(&dir, &config);
    assert_eq!(
        search.locate("base").unwrap(),
        dir.join("roles/base"),
        "a role beside the playbook is found first"
    );
    let err = search.locate("nosuchrole").unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("the role 'nosuchrole' was not found in "),
        "{text}"
    );
    assert!(
        text.contains(&dir.join("roles").display().to_string()),
        "{text}"
    );
    assert!(text.contains("/opt/roles"), "{text}");
    assert!(text.contains(&dir.display().to_string()), "{text}");
    assert_eq!(crate::stats::error_code(&err), 1, "{text}");
}

/// A three-part name is looked for in the collections, beside the playbook first.
#[test]
fn a_collection_role_is_found_under_ansible_collections() {
    let tmp = tree();
    let dir = tmp.path().to_path_buf();
    let role = dir.join("collections/ansible_collections/acme/demo/roles/hello");
    std::fs::create_dir_all(role.join("tasks")).unwrap();
    std::fs::write(role.join("tasks/main.yml"), "- debug: msg=hi\n").unwrap();
    let search = RoleSearch::new(&dir, &Config::default());
    assert_eq!(search.locate("acme.demo.hello").unwrap(), role);
}
