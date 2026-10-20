// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::super::{Templar, Vars};
use crate::testing::scratch;

/// A role laid out the way the reference measured one: `templates/`, `files/`, `tasks/`, and
/// `ansible_search_path` naming the role, its `tasks/` and the playbook directory, in that
/// order.
struct Role {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

/// The dispatch and the parity matrix read one list. A name the list does not hold is refused
/// before any term is read.
#[test]
fn the_supported_list_is_the_dispatch() {
    for name in [
        "env",
        "file",
        "vars",
        "pipe",
        "template",
        "first_found",
        "fileglob",
        "inventory_hostnames",
        "varnames",
    ] {
        assert!(super::supported(name), "{name}");
        assert!(
            super::supported(&format!("ansible.builtin.{name}")),
            "{name}"
        );
    }
    assert!(!super::supported("items"));
    assert!(!super::supported("community.general.env"));
    let t = crate::testing::templar();
    let err = t.render("{{ lookup('nosuch') }}", &Map::new()).unwrap_err();
    assert!(
        err.0
            .contains("lookup plugin (nosuch) is not available yet"),
        "{}",
        err.0
    );
}

impl Role {
    fn new() -> Self {
        let tmp = scratch();
        let dir = tmp.path().to_path_buf();
        for sub in ["role/templates", "role/files", "role/tasks"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        Self { _tmp: tmp, dir }
    }

    fn write(&self, rel: &str, text: &str) -> &Self {
        std::fs::write(self.dir.join(rel), text).unwrap();
        self
    }

    fn role(&self) -> PathBuf {
        self.dir.join("role")
    }

    fn vars(&self) -> Map<String, Value> {
        let role = self.role();
        let search = [role.clone(), role.join("tasks"), self.dir.clone()];
        let search: Vec<String> = search.iter().map(|p| p.display().to_string()).collect();
        json!({"ansible_search_path": search, "who": "me"})
            .as_object()
            .unwrap()
            .clone()
    }
}

fn templar(base: &Path) -> Templar {
    Templar::new(base.to_path_buf())
}

/// What `lookup('template')` hands back is never rendered again, as ansible-core 2.19.12 does
/// for `lookup('file')`.
#[test]
fn what_a_template_lookup_rendered_is_never_rendered_again() {
    let role = Role::new();
    role.write("role/templates/t.j2", "{{ '{{ 1 + 1 }}' }}");
    let t = templar(&role.dir);
    let mut map = role.vars();
    map.insert(
        "via_lookup".into(),
        json!("{{ lookup('template', 't.j2') }}"),
    );
    // `debug: var: via_lookup` reads the name out of the resolved map.
    let (resolved, untrusted) = t.resolve_vars_tainted(&map);
    assert_eq!(resolved["via_lookup"], json!("{{ 1 + 1 }}"));
    assert!(untrusted.contains("via_lookup"), "{untrusted:?}");
    // `debug: msg: "{{ via_lookup }}"` renders an argument against it.
    let vars = Vars {
        map: &resolved,
        hostvars: None,
        shared: None,
        facts: None,
        untrusted: Some(&untrusted),
        untrusted_hosts: None,
        base: None,
    };
    assert_eq!(
        t.render("{{ via_lookup }}", vars).unwrap(),
        json!("{{ 1 + 1 }}")
    );
    // And an argument that calls the lookup itself, before any variable holds it.
    assert_eq!(
        t.render("{{ lookup('template', 't.j2') }}", &map).unwrap(),
        json!("{{ 1 + 1 }}")
    );
}

/// Under a render that tracks no host text, as the `constructed` plugin's, a template lookup
/// keeps a host name that holds a template as text, also read through a variable.
#[test]
fn a_template_lookup_under_an_untracked_render_keeps_a_host_name_as_text() {
    let role = Role::new();
    role.write("role/templates/t.j2", "{{ x }}");
    let t = templar(&role.dir);
    let mut map = role.vars();
    map.insert("inventory_hostname".into(), json!("{{ 7 * 7 }}"));
    map.insert("x".into(), json!("{{ inventory_hostname }}"));
    assert_eq!(
        t.render_one_pass("{{ lookup('template', 't.j2') }}", &map)
            .unwrap(),
        json!("{{ 7 * 7 }}")
    );
}

/// `lookup('template', 'snip.j2')` finds the role's `templates/snip.j2` through
/// `ansible_search_path` and renders it with the task's variables.
#[test]
fn a_template_lookup_renders_the_role_s_template() {
    let role = Role::new();
    role.write("role/templates/snip.j2", "from role templates {{ who }}\n");
    let t = templar(&role.dir);
    assert_eq!(
        t.render("{{ lookup('template', 'snip.j2') }}", &role.vars())
            .unwrap(),
        json!("from role templates me\n")
    );
    // `<entry>/<name>` is the second place looked at, after `<entry>/templates/<name>`.
    role.write("role/bare.j2", "bare {{ who }}");
    assert_eq!(
        t.render("{{ lookup('template', 'bare.j2') }}", &role.vars())
            .unwrap(),
        json!("bare me")
    );
}

/// A template lookup whose template reads a name nobody set fails the way any undefined
/// read does, and one that names no file says which.
#[test]
fn a_template_lookup_fails_on_an_undefined_name_and_a_missing_file() {
    let role = Role::new();
    role.write("role/templates/u.j2", "{{ nobody_set_this }}");
    let t = templar(&role.dir);
    let err = t
        .render("{{ lookup('template', 'u.j2') }}", &role.vars())
        .unwrap_err();
    assert!(err.is_undefined(), "{err}");
    assert!(err.0.contains("in the template u.j2"), "{err}");
    let err = t
        .render("{{ lookup('template', 'none.j2') }}", &role.vars())
        .unwrap_err();
    assert!(
        err.0
            .contains("the template file none.j2 could not be found for the lookup"),
        "{err}"
    );
}

/// The delimiter options of the lookup have the bound of the `template` module's.
#[test]
fn a_template_lookup_refuses_a_very_long_delimiter() {
    let role = Role::new();
    role.write("role/templates/t.j2", "x {{ 1 }}\n");
    let t = templar(&role.dir);
    let long = "@".repeat(100_000);
    let err = t
        .render(
            &format!("{{{{ lookup('template', 't.j2', variable_start_string='{long}') }}}}"),
            &role.vars(),
        )
        .unwrap_err();
    assert!(
        err.0
            .contains("variable_start_string must not be longer than 64 characters"),
        "{err}"
    );
}

/// A template that looks itself up fails the task instead of overflowing the stack.
#[test]
fn a_template_that_looks_itself_up_fails() {
    let role = Role::new();
    role.write(
        "role/templates/self.j2",
        "{{ lookup('template', 'self.j2') }}",
    );
    let err = templar(&role.dir)
        .render("{{ lookup('template', 'self.j2') }}", &role.vars())
        .unwrap_err();
    assert!(err.0.contains("recursion"), "{err}");
}

/// A plain directory of files, for the absolute-pattern case `lookup('fileglob')` takes
/// straight: no role, no search path, just a controller directory the pattern already names.
struct Dir {
    _tmp: tempfile::TempDir,
    path: PathBuf,
}

impl Dir {
    fn new() -> Self {
        let tmp = scratch();
        let path = tmp.path().to_path_buf();
        Self { _tmp: tmp, path }
    }

    fn write(&self, name: &str) -> &Self {
        std::fs::write(self.path.join(name), "x").unwrap();
        self
    }

    /// A directory (not a file) under the fixture, so a pattern that matches its name still
    /// must not match it as a result.
    fn mkdir(&self, name: &str) -> &Self {
        std::fs::create_dir_all(self.path.join(name)).unwrap();
        self
    }

    fn pattern(&self, glob: &str) -> String {
        format!("{}/{glob}", self.path.display())
    }

    /// The full paths of `names`, in the order the directory listing gives them: the order
    /// `glob.glob` answers in, whatever the filesystem.
    fn listed(&self, names: &[&str]) -> Vec<String> {
        std::fs::read_dir(&self.path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| names.iter().any(|n| name.to_str() == Some(n)))
            .map(|name| self.path.join(name).display().to_string())
            .collect()
    }
}

/// A dot file matches only a pattern that starts with a dot: `*`, `?h*` and `[.]h*` skip
/// `.h.txt`, while `.*` and the literal name find it.
#[test]
fn fileglob_skips_dot_files_unless_the_pattern_starts_with_a_dot() {
    let dir = Dir::new();
    dir.write(".h.txt").write("a.txt");
    let t = Templar::new(dir.path.clone());
    let hidden = dir.path.join(".h.txt").display().to_string();
    for (glob, want) in [
        ("*", json!([dir.path.join("a.txt").display().to_string()])),
        ("?h*", json!([])),
        ("[.]h*", json!([])),
        (".*", json!([hidden])),
        (".h.txt", json!([hidden])),
    ] {
        let pattern = dir.pattern(glob);
        assert_eq!(
            t.render(
                &format!("{{{{ lookup('fileglob', '{pattern}', wantlist=True) }}}}"),
                &Map::new()
            )
            .unwrap(),
            want,
            "{glob}"
        );
    }
}

/// `fileglob` returns matches in directory-listing order, with no sort and no dedupe.
#[test]
fn fileglob_keeps_the_directory_listing_order() {
    let dir = Dir::new();
    // Written out of order (every seventh letter), so neither a listing in creation order nor
    // one in reverse creation order (tmpfs) is sorted.
    let names: Vec<String> = (0..26u8)
        .map(|i| format!("{}.txt", char::from(b'a' + i * 7 % 26)))
        .collect();
    for name in &names {
        dir.write(name);
    }
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let t = Templar::new(dir.path.clone());
    let all = dir.pattern("*.txt");
    assert_eq!(
        t.render(
            &format!("{{{{ lookup('fileglob', '{all}', wantlist=True) }}}}"),
            &Map::new()
        )
        .unwrap(),
        json!(dir.listed(&names))
    );
}

/// The two static `with_fileglob` patterns of the `airgap` role match files only, in listing
/// order: a list with `wantlist=True`, a comma join without.
#[test]
fn fileglob_matches_the_two_static_airgap_patterns() {
    let dir = Dir::new();
    dir.write("container-selinux-1.rpm")
        .write("k3s-selinux-1.rpm")
        .write("k3s-selinux-1.el8.rpm")
        .write("other.txt")
        .mkdir("k3s-selinux-dir.rpm");
    let t = Templar::new(dir.path.clone());
    let rpm = dir.pattern("k3s-selinux*.rpm");
    assert_eq!(
        t.render(
            &format!("{{{{ lookup('fileglob', '{rpm}', wantlist=True) }}}}"),
            &Map::new()
        )
        .unwrap(),
        json!(dir.listed(&["k3s-selinux-1.el8.rpm", "k3s-selinux-1.rpm"]))
    );

    dir.write("images-2.tar.gz")
        .write("images-1.tar.gz")
        .mkdir("images-dir.tar.gz");
    let images = dir.pattern("*.tar.gz");
    assert_eq!(
        t.render(
            &format!("{{{{ lookup('fileglob', '{images}') }}}}"),
            &Map::new()
        )
        .unwrap(),
        json!(
            dir.listed(&["images-1.tar.gz", "images-2.tar.gz"])
                .join(",")
        )
    );
}

/// A pattern with no directory prefers `files/` of the search path over the entry itself, which
/// counts only when `files/` has nothing.
#[test]
fn fileglob_prefers_files_over_the_entry_itself() {
    let role = Role::new();
    role.write("role/files/only-in-files.conf", "f")
        .write("role/only-in-role.conf", "r")
        .write("role/both.conf", "role's own")
        .write("role/files/both.conf", "files' own");
    let t = templar(&role.dir);
    let vars = role.vars();
    assert_eq!(
        t.render("{{ lookup('fileglob', 'only-in-files.conf') }}", &vars)
            .unwrap(),
        json!(
            role.role()
                .join("files/only-in-files.conf")
                .display()
                .to_string()
        )
    );
    assert_eq!(
        t.render("{{ lookup('fileglob', 'only-in-role.conf') }}", &vars)
            .unwrap(),
        json!(role.role().join("only-in-role.conf").display().to_string())
    );
    assert_eq!(
        t.render("{{ lookup('fileglob', 'both.conf') }}", &vars)
            .unwrap(),
        json!(role.role().join("files/both.conf").display().to_string())
    );
}

/// A `fileglob` with no match anywhere answers an empty list, never an error.
#[test]
fn fileglob_with_no_match_is_an_empty_list_not_an_error() {
    let role = Role::new();
    let t = templar(&role.dir);
    assert_eq!(
        t.render("{{ lookup('fileglob', 'nope*.txt') }}", &role.vars())
            .unwrap(),
        json!([])
    );
}

/// `lookup()` joins several string results with a comma, keeps the list when one is not a
/// string, and `wantlist=True` always keeps it.
#[test]
fn lookup_joins_several_string_results_with_a_comma() {
    let t = crate::testing::templar();
    let vars = json!({"x": "a", "y": "b", "l": [1]});
    let r = |text: &str| t.render(text, vars.as_object().unwrap()).unwrap();
    assert_eq!(r("{{ lookup('vars', 'x', 'y') }}"), json!("a,b"));
    assert_eq!(r("{{ lookup('vars', 'x', 'l') }}"), json!(["a", [1]]));
    assert_eq!(r("{{ lookup('vars', 'x') }}"), json!("a"));
    assert_eq!(
        r("{{ lookup('vars', 'x', 'y', wantlist=True) }}"),
        json!(["a", "b"])
    );
    assert_eq!(r("{{ lookup('vars', 'x', wantlist=True) }}"), json!(["a"]));
}

/// `ansible.builtin.fileglob` answers exactly as the bare name does.
#[test]
fn fileglob_answers_under_its_ansible_builtin_alias_too() {
    let role = Role::new();
    role.write("role/files/x.conf", "x");
    let t = templar(&role.dir);
    assert_eq!(
        t.render(
            "{{ lookup('ansible.builtin.fileglob', 'x.conf') }}",
            &role.vars()
        )
        .unwrap(),
        json!(role.role().join("files/x.conf").display().to_string())
    );
}

/// The five `first_found` lines measured on ansible-core 2.19.12 in a role holding
/// `files/second.txt` and `templates/snip.j2`.
#[test]
fn first_found_tries_each_name_against_the_search_path_only() {
    let role = Role::new();
    role.write("role/files/second.txt", "2")
        .write("role/templates/snip.j2", "x");
    let t = templar(&role.dir);
    let vars = role.vars();
    let found = role.role().join("files/second.txt").display().to_string();
    let r = |text: &str| t.render(text, &vars);
    let not_found = |text: &str| {
        let err = r(text).unwrap_err().0;
        assert!(
            err.contains("No file was found when using first_found."),
            "{text}: {err}"
        );
    };
    not_found("{{ lookup('first_found', ['nope.txt', 'second.txt']) }}");
    assert_eq!(
        r("{{ lookup('first_found', ['nope.txt', 'files/second.txt']) }}").unwrap(),
        json!(found)
    );
    assert_eq!(
        r("{{ lookup('first_found', {'files': ['nope.txt', 'second.txt'], 'paths': ['files']}) }}")
            .unwrap(),
        json!(found)
    );
    not_found("{{ lookup('first_found', ['nope.j2', 'snip.j2']) }}");
    // `skip: true` turns the failure into an empty list, in the mapping or as a keyword.
    assert_eq!(
        r("{{ lookup('first_found', {'files': ['nope.txt'], 'skip': true}) }}").unwrap(),
        json!([])
    );
    assert_eq!(
        r("{{ lookup('first_found', 'nope.txt', skip=true) }}").unwrap(),
        json!([])
    );
    // An absolute name is taken as it stands, and `,` `;` split a name list.
    assert_eq!(
        r(&format!(
            "{{{{ lookup('first_found', 'nope.txt;{found}') }}}}"
        ))
        .unwrap(),
        json!(found)
    );
}

/// `_process_terms` joins a string term to the plugin's `paths` option, so the keyword applies
/// to plain names as the mapping's `paths` does.
#[test]
fn first_found_joins_the_paths_keyword_and_a_mapping_resets_the_options() {
    let role = Role::new();
    role.write("role/files/second.txt", "2");
    let t = templar(&role.dir);
    let vars = role.vars();
    let found = role.role().join("files/second.txt").display().to_string();
    assert_eq!(
        t.render(
            "{{ lookup('first_found', 'second.txt', paths=['files']) }}",
            &vars
        )
        .unwrap(),
        json!(found)
    );
    let err = t
        .render(
            "{{ lookup('first_found', {'files': ['nope.txt']}, skip=true) }}",
            &vars,
        )
        .unwrap_err();
    assert!(
        err.0.contains("No file was found when using first_found."),
        "{err}"
    );
}

/// The reference templates a variable when it is read, so a template read through the lookup
/// sees `a: "{{ b }}"` as `1` even while the task's variables are still being resolved.
#[test]
fn a_template_lookup_reads_a_chained_variable_resolved() {
    let role = Role::new();
    role.write("role/templates/chain.j2", "{{ a }}");
    let t = templar(&role.dir);
    let mut map = role.vars();
    map.insert("a".into(), json!("{{ b }}"));
    map.insert("b".into(), json!(1));
    map.insert("via".into(), json!("{{ lookup('template', 'chain.j2') }}"));
    let (resolved, untrusted) = t.resolve_vars_tainted(&map);
    assert_eq!(resolved["via"], json!("1"));
    assert!(untrusted.contains("via"), "{untrusted:?}");

    let from_host = BTreeSet::from(["a".to_string()]);
    let vars = Vars {
        map: &map,
        hostvars: None,
        shared: None,
        facts: None,
        untrusted: Some(&from_host),
        untrusted_hosts: None,
        base: None,
    };
    assert_eq!(
        t.render("{{ lookup('template', 'chain.j2') }}", vars)
            .unwrap(),
        json!("{{ b }}")
    );
}

/// A name that cannot be resolved reads as undefined and fails only when it is actually read,
/// never in a branch that does not run.
#[test]
fn a_name_that_cannot_be_resolved_fails_only_when_it_is_read() {
    let role = Role::new();
    role.write(
        "role/templates/branch.j2",
        "{% if use_proxy %}{{ proxy }}{% endif %}ok",
    )
    .write("role/templates/default.j2", "{{ proxy | default('none') }}")
    .write("role/templates/defined.j2", "{{ proxy is defined }}")
    .write("role/templates/read.j2", "{{ proxy }}");
    let t = templar(&role.dir);
    let mut map = role.vars();
    map.insert("use_proxy".into(), json!(false));
    map.insert("proxy".into(), json!("{{ proxy_host }}:3128"));
    let r = |name: &str| t.render(&format!("{{{{ lookup('template', '{name}') }}}}"), &map);
    assert_eq!(r("branch.j2").unwrap(), json!("ok"));
    assert_eq!(r("default.j2").unwrap(), json!("none"));
    assert_eq!(r("defined.j2").unwrap(), json!("False"));
    let err = r("read.j2").unwrap_err();
    assert!(err.is_undefined(), "{err}");
}

/// A `template` lookup gives the template `template_path`, `template_fullpath` and, unless it
/// exists, `ansible_managed`.
#[test]
fn a_template_lookup_sets_the_template_variables() {
    let role = Role::new();
    role.write(
        "role/templates/m.j2",
        "{{ ansible_managed | comment }}\n{{ template_path }} {{ template_fullpath }}\n",
    );
    let t = templar(&role.dir);
    let full = role.role().join("templates/m.j2").display().to_string();
    assert_eq!(
        t.render("{{ lookup('template', 'm.j2') }}", &role.vars())
            .unwrap(),
        json!(format!("#\n# Ansible managed\n#\nm.j2 {full}\n"))
    );
    let mut map = role.vars();
    map.insert("ansible_managed".into(), json!("by hand"));
    assert_eq!(
        t.render("{{ lookup('template', 'm.j2') }}", &map).unwrap(),
        json!(format!("#\n# by hand\n#\nm.j2 {full}\n"))
    );
}
