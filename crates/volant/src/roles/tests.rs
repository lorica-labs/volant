// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::testing::scratch;
use rstest::rstest;

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
    let role = load(&dir.join("roles/base"), "base", &RoleFrom::default()).unwrap();
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
    let role = load(&dir.join("roles/base"), "base", &RoleFrom::default()).unwrap();
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
    let err = load(&dir.join("roles/base"), "base", &from).unwrap_err();
    assert!(
        format!("{err:#}").contains("Could not find specified file in role: tasks/nosuch"),
        "{err:#}"
    );
    std::fs::create_dir_all(dir.join("roles/bare/defaults")).unwrap();
    std::fs::write(dir.join("roles/bare/defaults/main.yml"), "q: 1\n").unwrap();
    let role = load(&dir.join("roles/bare"), "bare", &RoleFrom::default()).unwrap();
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

/// The directory of a playbook given as `play/../play/./sub` is searched by its folded name, the
/// way `os.path.abspath` gives it in ansible-core 2.19.12.
#[test]
fn the_playbook_directory_is_searched_by_its_folded_name() {
    let search = RoleSearch::new(Path::new("/w/other/../play/./sub"), &Config::default());
    assert_eq!(search.paths[0], Path::new("/w/play/sub/roles"));
    assert_eq!(search.paths.last().unwrap(), Path::new("/w/play/sub"));
    assert_eq!(search.collections[0], Path::new("/w/play/sub/collections"));
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

/// A role `rr` whose directories hold one file each of the shapes the tables below name, and two
/// files beside it that are in no role. Every file says which it is: a task by its name, a
/// variable by its value.
fn escape_tree() -> tempfile::TempDir {
    let tmp = scratch();
    let root = tmp.path();
    let write = |path: &str, text: &str| {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    let task = |name: &str| format!("- name: ran-{name}\n  debug: msg=x\n");
    let var = |name: &str| format!("seen_{name}: yes\n");
    write("roles/rr/tasks/main.yml", &task("main"));
    write("roles/rr/tasks/install.yml", &task("install"));
    write("roles/rr/tasks/two.yaml", &task("two"));
    write(
        "roles/rr/tasks/data.json",
        r#"[{"name": "ran-data", "debug": {"msg": "x"}}]"#,
    );
    write("roles/rr/tasks/bare", &task("bare"));
    write("roles/rr/tasks/sub/x.yml", &task("sub-x"));
    write("roles/rr/tasks/sub/main.yml", &task("sub-main"));
    write("roles/rr/tasks/d/a.yml", &task("d-a"));
    write("roles/rr/vars/main.yml", &var("main"));
    write("roles/rr/vars/extra.yml", &var("extra"));
    write("roles/rr/vars/vd/a.yml", &var("vd"));
    write("roles/rr/handlers/main.yml", &task("handler"));
    write("roles/rr/defaults/main.yml", &var("default"));
    write("roles/bare/tasks/main.yml", &task("bare-role"));
    write("outside_tasks.yml", &task("outside"));
    write("outside_vars.yml", &var("outside"));
    write("outsidedir/x.yml", &task("outside-dir-x"));
    #[cfg(unix)]
    {
        let link = |target: &str, at: &str| {
            std::os::unix::fs::symlink(target, root.join(at)).unwrap();
        };
        link("install.yml", "roles/rr/tasks/link_in.yml");
        link("../../../outside_tasks.yml", "roles/rr/tasks/link_out.yml");
        link("sub", "roles/rr/tasks/linkdir_in");
        link("../../../outsidedir", "roles/rr/tasks/linkdir_out");
        link("../../../outside_vars.yml", "roles/rr/vars/vlink_out.yml");
        link("../../../../outside_vars.yml", "roles/rr/vars/vd/o.yml");
        link("../../../outsidedir", "roles/rr/vars/vdl");
        link("../../../outsidedir", "roles/rr/defaults/dl");
        link("../../../outsidedir", "roles/rr/handlers/hl");
    }
    tmp
}

/// A `*_from` name is accepted when its folded path lies under the matching role directory,
/// as `_load_role_yaml` of ansible-core 2.19.12 does; `{root}` is the temporary tree.
#[rstest]
// Plain names, with and without the extension, in a sub-directory, and through a `..` that comes
// back in.
#[case("tasks", "install", Ok("ran-install"))]
#[case("tasks", "install.yml", Ok("ran-install"))]
#[case("tasks", "two", Ok("ran-two"))]
#[case("tasks", "two.yaml", Ok("ran-two"))]
#[case("tasks", "data", Ok("ran-data"))]
#[case("tasks", "bare", Ok("ran-bare"))]
#[case("tasks", "sub/x", Ok("ran-sub-x"))]
#[case("tasks", "sub/main", Ok("ran-sub-main"))]
#[case("tasks", "sub/../install", Ok("ran-install"))]
#[case("tasks", "../tasks/install", Ok("ran-install"))]
#[case("tasks", "{root}/roles/rr/tasks/install", Ok("ran-install"))]
// A directory is not a tasks file, whatever it holds; the reference reads one only for variables.
#[case("tasks", "d", Err("Could not find specified file in role: tasks/d"))]
#[case(
    "tasks",
    "sub",
    Err("Could not find specified file in role: tasks/sub")
)]
#[case("tasks", ".", Err("Could not find specified file in role: tasks/."))]
#[case("tasks", "..", Err("Could not find specified file in role: tasks/.."))]
#[case(
    "tasks",
    "nosuch",
    Err("Could not find specified file in role: tasks/nosuch")
)]
#[case(
    "tasks",
    "/nosuch",
    Err("Could not find specified file in role: tasks//nosuch")
)]
#[case(
    "tasks",
    "../../../nosuch",
    Err("Could not find specified file in role: tasks/../../../nosuch")
)]
// Out of the directory: refused, the file is never read.
#[case(
    "tasks",
    "../../../outside_tasks",
    Err(
        "Failed loading '{root}/roles/rr/tasks/../../../outside_tasks.yml' for role (rr) as it is not inside the expected role path: '{root}/roles/rr/tasks'"
    )
)]
#[case(
    "tasks",
    "../../../outside_tasks.yml",
    Err(
        "Failed loading '{root}/roles/rr/tasks/../../../outside_tasks.yml' for role (rr) as it is not inside the expected role path: '{root}/roles/rr/tasks'"
    )
)]
#[case(
    "tasks",
    "{root}/outside_tasks",
    Err(
        "Failed loading '{root}/outside_tasks.yml' for role (rr) as it is not inside the expected role path: '{root}/roles/rr/tasks'"
    )
)]
#[case(
    "tasks",
    "../vars/main",
    Err(
        "Failed loading '{root}/roles/rr/tasks/../vars/main.yml' for role (rr) as it is not inside the expected role path: '{root}/roles/rr/tasks'"
    )
)]
#[case(
    "vars",
    "../../../outside_vars",
    Err(
        "Failed loading '{root}/roles/rr/vars/../../../outside_vars.yml' for role (rr) as it is not inside the expected role path: '{root}/roles/rr/vars'"
    )
)]
#[case(
    "vars",
    "{root}/outside_vars",
    Err(
        "Failed loading '{root}/outside_vars.yml' for role (rr) as it is not inside the expected role path: '{root}/roles/rr/vars'"
    )
)]
#[case(
    "vars",
    "../tasks/main",
    Err(
        "Failed loading '{root}/roles/rr/vars/../tasks/main.yml' for role (rr) as it is not inside the expected role path: '{root}/roles/rr/vars'"
    )
)]
#[case(
    "defaults",
    "../../../outside_vars",
    Err(
        "Failed loading '{root}/roles/rr/defaults/../../../outside_vars.yml' for role (rr) as it is not inside the expected role path: '{root}/roles/rr/defaults'"
    )
)]
#[case(
    "handlers",
    "../../../outside_tasks",
    Err(
        "Failed loading '{root}/roles/rr/handlers/../../../outside_tasks.yml' for role (rr) as it is not inside the expected role path: '{root}/roles/rr/handlers'"
    )
)]
// Variables: a name, a name with its extension, a directory read whole, and a name that comes back
// in through a `..`.
#[case("vars", "extra", Ok("seen_extra"))]
#[case("vars", "extra.yml", Ok("seen_extra"))]
#[case("vars", "vd/a", Ok("seen_vd"))]
#[case("vars", "vd/../extra", Ok("seen_extra"))]
#[case("vars", "{root}/roles/rr/vars/extra", Ok("seen_extra"))]
#[case(
    "vars",
    "nosuch",
    Err("Could not find specified file in role: vars/nosuch")
)]
#[case("defaults", "main", Ok("seen_default"))]
#[case("handlers", "main", Ok("ran-handler"))]
fn a_from_option_names_a_file_inside_its_directory(
    #[case] sub: &str,
    #[case] from: &str,
    #[case] want: Result<&str, &str>,
) {
    check_from(sub, from, want);
}

/// Loads the `rr` role of [`escape_tree`] with one selector set, and compares the outcome.
fn check_from(sub: &str, from: &str, want: Result<&str, &str>) {
    let tmp = escape_tree();
    let root = tmp.path().display().to_string();
    let from = from.replace("{root}", &root);
    let mut selector = RoleFrom::default();
    match sub {
        "tasks" => selector.tasks = from,
        "vars" => selector.vars = from,
        "defaults" => selector.defaults = from,
        _ => selector.handlers = from,
    }
    let loaded = load(&tmp.path().join("roles/rr"), "rr", &selector);
    match want {
        Ok(seen) => {
            let role = loaded.unwrap_or_else(|e| panic!("{sub}_from {selector:?}: {e:#}"));
            let text = format!(
                "{:?} {:?} {:?} {:?}",
                role.tasks, role.handlers, role.vars, role.defaults
            );
            assert!(text.contains(seen), "{sub}: {seen} is not in {text}");
        }
        Err(refusal) => {
            let err = loaded.expect_err("the name must be refused");
            assert_eq!(format!("{err:#}"), refusal.replace("{root}", &root));
        }
    }
}

/// The reference folds the text of the path and never resolves a link, so a link inside the
/// directory is followed wherever it points. Measured on ansible-core 2.19.12.
#[cfg(unix)]
#[rstest]
#[case("tasks", "link_in", Ok("ran-install"))]
#[case("tasks", "linkdir_in/x", Ok("ran-sub-x"))]
#[case("tasks", "link_out", Ok("ran-outside"))]
#[case("tasks", "link_out.yml", Ok("ran-outside"))]
#[case("tasks", "linkdir_out/x", Ok("ran-outside-dir-x"))]
#[case(
    "tasks",
    "linkdir_out",
    Err("Could not find specified file in role: tasks/linkdir_out")
)]
#[case("vars", "vlink_out", Ok("seen_outside"))]
// A directory of variables is read whole: its link to a file outside is read as well.
#[case("vars", "vd", Ok("seen_outside"))]
// A `..` after a link to a directory is folded out of the text first, as the reference does, so it
// never climbs out through the link. The file the kernel would reach that way is not found.
#[case("tasks", "linkdir_in/../install", Ok("ran-install"))]
#[case(
    "tasks",
    "linkdir_out/../outside_tasks",
    Err("Could not find specified file in role: tasks/linkdir_out/../outside_tasks")
)]
#[case(
    "vars",
    "vdl/../outside_vars",
    Err("Could not find specified file in role: vars/vdl/../outside_vars")
)]
#[case(
    "defaults",
    "dl/../outside_vars",
    Err("Could not find specified file in role: defaults/dl/../outside_vars")
)]
#[case(
    "handlers",
    "hl/../outside_tasks",
    Err("Could not find specified file in role: handlers/hl/../outside_tasks")
)]
fn a_link_inside_the_directory_is_followed(
    #[case] sub: &str,
    #[case] from: &str,
    #[case] want: Result<&str, &str>,
) {
    check_from(sub, from, want);
}

/// A role with no directory of a kind has nothing to read there, whatever the entry names: the
/// reference does not look, and so does not refuse. Measured on ansible-core 2.19.12.
#[test]
fn a_missing_directory_is_not_looked_into() {
    let tmp = escape_tree();
    let selector = RoleFrom {
        defaults: "../../../outside_vars".to_string(),
        handlers: "nosuch".to_string(),
        ..RoleFrom::default()
    };
    let role = load(&tmp.path().join("roles/bare"), "bare", &selector).unwrap();
    assert!(role.defaults.is_empty() && role.handlers.is_empty());
}

/// `meta/argument_specs` with no extension is read only beside a `.yml`, `.yaml` or `.json`
/// one, and then wins over it, as `_load_argument_specs` of ansible-core 2.19.12 does.
#[rstest]
#[case(&["argument_specs"], None)]
#[case(&["argument_specs.yml"], Some("from-yml"))]
#[case(&["argument_specs", "argument_specs.yml"], Some("from-bare"))]
fn argument_specs_are_read_the_way_the_reference_reads_them(
    #[case] present: &[&str],
    #[case] want: Option<&str>,
) {
    let tmp = scratch();
    let role = tmp.path().join("rr");
    std::fs::create_dir_all(role.join("meta")).unwrap();
    for name in present {
        let from = if name.contains('.') {
            "from-yml"
        } else {
            "from-bare"
        };
        let text = format!("argument_specs:\n  main:\n    short_description: {from}\n");
        std::fs::write(role.join("meta").join(name), text).unwrap();
    }
    let (_, description) = argument_spec(&role, "main").unwrap();
    assert_eq!(description.as_deref(), want);
}
