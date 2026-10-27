// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};

use rstest::rstest;

use super::*;

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

/// `-p PATH` installs under `PATH/ansible_collections`, unless `PATH` already is that directory.
#[test]
fn the_collections_root() {
    assert_eq!(
        collections_root(Path::new("/work/cols")),
        PathBuf::from("/work/cols/ansible_collections")
    );
    assert_eq!(
        collections_root(Path::new("/work/cols/ansible_collections")),
        PathBuf::from("/work/cols/ansible_collections")
    );
}

/// Measured on ansible-core 2.19.12 with `-p /work/cols` and the default collections paths.
#[test]
fn a_collections_path_outside_the_configured_ones_is_warned_about() {
    let configured = paths(&[
        "/home/u/.ansible/collections",
        "/usr/share/ansible/collections",
    ]);
    assert_eq!(
        collections_path_warning(Path::new("/work/cols"), &configured).as_deref(),
        Some(
            "The specified collections path '/work/cols' is not part of the configured Ansible collections paths '/home/u/.ansible/collections:/usr/share/ansible/collections'. The installed collection will not be picked up in an Ansible run, unless within a playbook-adjacent collections directory."
        )
    );
    for inside in [
        "/home/u/.ansible/collections",
        "/home/u/.ansible",
        "/usr/share/ansible/collections/ansible_collections",
    ] {
        assert_eq!(
            collections_path_warning(Path::new(inside), &configured),
            None,
            "{inside}"
        );
    }
}

#[test]
fn the_roles_paths_put_p_first() {
    let configured = paths(&["/home/u/.ansible/roles", "/etc/ansible/roles"]);
    assert_eq!(roles_paths(None, &configured), configured);
    let with = roles_paths(Some("/work/roles"), &configured);
    assert_eq!(with[0], PathBuf::from("/work/roles"));
    assert_eq!(with[1..], configured[..]);
}

/// Measured on ansible-core 2.19.12: `role list -p /tmp/gm/r` with none of the paths there warns
/// for each, then fails with exit 5.
#[test]
fn every_missing_roles_path_is_warned_about() {
    let dir = tempfile::tempdir().expect("a directory");
    let there = dir.path().to_path_buf();
    let gone = dir.path().join("gone");
    let (warnings, failure) = missing_roles_paths(&[gone.clone(), there]);
    assert_eq!(
        warnings,
        [format!(
            "- the configured path {} does not exist.",
            gone.display()
        )]
    );
    assert_eq!(failure, None);
    let (warnings, failure) = missing_roles_paths(std::slice::from_ref(&gone));
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        failure,
        Some(Failure::options(
            "- None of the provided paths were usable. Please specify a valid path with --roles-path"
        ))
    );
}

#[rstest]
#[case::both(&["a.b"], Some("x.yml"), Failure::error("The positional collection_name arg and --requirements-file are mutually exclusive."))]
#[case::neither(&[], None, Failure::error("You must specify a collection name or a requirements file."))]
#[case::missing_file(&[], Some("/nonexistent/nope.yml"), Failure::error("The requirements file '/nonexistent/nope.yml' does not exist."))]
fn the_collection_requirement_errors(
    #[case] names: &[&str],
    #[case] file: Option<&str>,
    #[case] want: Failure,
) {
    let names: Vec<String> = names.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(one_of_collections_requirements(&names, file), Err(want));
}

#[rstest]
#[case::neither(&[], None, Failure::options("- you must specify a user/role name or a roles file"))]
#[case::not_yaml(&[], Some("roles.txt"), Failure::error("Invalid role requirements file, it must end with a .yml or .yaml extension"))]
#[case::missing_file(&[], Some("/nonexistent/nope.yml"), Failure::error("The requirements file '/nonexistent/nope.yml' does not exist."))]
fn the_role_requirement_errors(
    #[case] names: &[&str],
    #[case] file: Option<&str>,
    #[case] want: Failure,
) {
    let names: Vec<String> = names.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(one_of_roles_requirements(&names, file), Err(want));
}

#[test]
fn names_alone_need_no_file() {
    let names = vec!["a.b".to_string()];
    assert_eq!(one_of_collections_requirements(&names, None), Ok(None));
    assert_eq!(one_of_roles_requirements(&names, None), Ok(None));
}
