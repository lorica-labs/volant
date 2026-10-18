// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;

use super::*;

#[test]
fn builtin_prefixes_are_stripped_and_collections_kept() {
    assert_eq!(short_name("ansible.builtin.command"), "command");
    assert_eq!(short_name("ansible.legacy.shell"), "shell");
    assert_eq!(short_name("command"), "command");
    assert_eq!(short_name("community.general.ufw"), "community.general.ufw");
}

#[test]
fn native_lookup_uses_the_short_name() {
    assert_eq!(native("ansible.builtin.raw").map(|m| m.name), Some("raw"));
    assert!(native("file").is_none(), "file is not native yet");
    assert!(
        native("community.general.command").is_none(),
        "another collection's command is not ours"
    );
}

#[test]
fn names_are_unique_and_sorted() {
    for table in [NATIVE_MODULES, LOCAL_MODULES] {
        let names: Vec<&str> = table.iter().map(|m| m.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }
    let mut all: Vec<&str> = NATIVE_MODULES
        .iter()
        .chain(LOCAL_MODULES)
        .map(|m| m.name)
        .collect();
    let total = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), total, "a module name is in both tables");
}

#[test]
fn only_the_two_tables_are_known() {
    assert!(is_known("ansible.builtin.shell"));
    assert!(is_known("set_fact"));
    assert!(is_known("ansible.legacy.debug"));
    assert!(!is_known("nosuchmodule"));
    assert!(!is_known("file"), "not implemented yet, so not known");
    assert!(!is_known("community.general.debug"));
}

/// The three dynamic statements, each in the table that answers for it.
#[test]
fn the_three_dynamic_statements_are_each_in_one_table() {
    assert!(is_known("include_vars"));
    assert!(is_known("ansible.builtin.include_vars"));
    assert!(include_module("include_vars").is_none());
    for statement in ["include_tasks", "include_role"] {
        assert!(include_module(statement).is_some(), "{statement}");
        assert!(!is_known(statement), "{statement}");
    }
    assert_eq!(include_module("include_tasks"), Some(true));
    assert_eq!(include_module("ansible.builtin.include_role"), Some(false));
}

/// The three states a module name can be in have to stay three.
#[test]
fn a_builtin_is_told_apart_from_a_name_that_resolves_to_nothing() {
    for yes in [
        "lineinfile",
        "ansible.builtin.lineinfile",
        "ansible.legacy.file",
        "command",
    ] {
        assert!(is_builtin(yes), "{yes}");
    }
    for no in [
        "nosuchmodule",
        "ansible.builtin.nosuchmodule",
        "community.general.ufw",
        "community.general.command",
    ] {
        assert!(!is_builtin(no), "{no}");
    }
    assert!(
        !is_known("lineinfile") && is_builtin("lineinfile"),
        "a builtin we have not written is neither runnable nor a typo"
    );
}

/// The builtin table is sorted and holds every native and controller-side module under its
/// `ansible.builtin` name.
#[test]
fn the_builtin_table_is_sorted_and_covers_both_registries() {
    let mut sorted = BUILTIN_MODULES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(BUILTIN_MODULES, sorted.as_slice());
    for m in NATIVE_MODULES.iter().chain(LOCAL_MODULES) {
        assert!(
            BUILTIN_MODULES.contains(&m.name),
            "{} is not a name ansible.builtin has",
            m.name
        );
    }
    for table in [IMPORT_MODULES, INCLUDE_MODULES] {
        let names: Vec<&str> = table.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted);
        for name in names {
            assert!(BUILTIN_MODULES.contains(&name), "{name}");
            assert!(
                !is_known(name),
                "{name} names work rather than being it, so nothing runs it as a module"
            );
        }
    }
}

/// Every argument list is sorted, free of duplicates and holds each name the reference
/// validates, so no supported name is quoted as unsupported.
#[test]
fn every_argument_list_is_sorted_and_covers_what_the_reference_validates() {
    for m in NATIVE_MODULES.iter().chain(LOCAL_MODULES) {
        let names: Vec<&str> = m.args.iter().map(|a| a.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted, "{}", m.name);
        assert!(
            m.validated_as.is_none() || !names.is_empty(),
            "{} validates against an empty list",
            m.name
        );
    }
    // Measured on ansible-core 2.19.12, through `args:` on both modules: one module name,
    // one list of names, and one refusal. The ad-hoc path validates nothing, because
    // `free_form` swallows the whole line into `_raw_params`.
    let names = |m: &ModuleSpec| m.args.iter().map(|a| a.name).collect::<Vec<_>>();
    assert_eq!(
        names(&COMMAND),
        names(&SHELL),
        "the reference shares the module"
    );
    assert_eq!(SHELL.validated_as, Some("ansible.legacy.command"));
    assert_eq!(
        RAW.validated_as, None,
        "raw's action plugin never validates an argument"
    );
}

/// `executable` names the shell a command line is handed to, so it means something only where a
/// shell runs.
#[test]
fn executable_is_read_only_where_a_shell_runs() {
    let status = |m: &ModuleSpec| {
        m.args
            .iter()
            .find(|a| a.name == "executable")
            .map(|a| a.status)
    };
    assert_eq!(status(&COMMAND), Some(ArgStatus::Inert));
    assert_eq!(status(&SHELL), Some(ArgStatus::Honoured));
    assert_eq!(status(&RAW), Some(ArgStatus::Honoured));
    let page = documentation_table();
    let row = |name: &str| {
        page.lines()
            .find(|l| l.starts_with(&format!("| `{name}` |")))
            .unwrap_or_default()
            .to_string()
    };
    assert!(!row("command").contains("executable"), "{}", row("command"));
    assert!(row("shell").contains("`executable`"), "{}", row("shell"));
}

/// `expand_argument_vars` is read by `command` and refused by name on `shell`, as the reference does.
#[test]
fn expand_argument_vars_is_read_on_command_and_refused_on_shell() {
    let status = |m: &ModuleSpec| {
        m.args
            .iter()
            .find(|a| a.name == "expand_argument_vars")
            .map(|a| a.status)
    };
    assert_eq!(status(&COMMAND), Some(ArgStatus::Honoured));
    assert_eq!(status(&SHELL), Some(ArgStatus::Refused(None)));
    assert_eq!(status(&RAW), None);
    let page = documentation_table();
    assert!(
        page.contains(
            "- `shell`: `expand_argument_vars`
"
        ),
        "{page}"
    );
    assert!(!page.contains("- `command`:"), "{page}");
}

/// The page says which arguments are expanded and that a `command` expands its words.
#[test]
fn the_page_says_which_arguments_are_expanded() {
    let page = documentation_table();
    for phrase in [
        "`creates` and `removes` as paths and expands `~` and `$VAR`",
        "`command: /bin/echo $HOME` prints the home directory",
    ] {
        assert!(
            page.contains(phrase),
            "{phrase} is missing from:
{page}"
        );
    }
}

/// A `type=bool` argument reads every spelling the reference takes: `"false"`, `"no"`, `0` and
/// `"Off"` all turn it off.
#[test]
fn a_boolean_argument_reads_every_spelling_the_reference_takes() {
    for yes in [
        json!(true),
        json!("true"),
        json!("True"),
        json!("YES"),
        json!("t"),
        json!("on"),
        json!(1),
        json!(1.0),
        json!(" true"),
    ] {
        assert_eq!(arg_bool(&yes), Some(true), "{yes}");
    }
    for no in [
        json!(false),
        json!("false"),
        json!("no"),
        json!("Off"),
        json!("f"),
        json!(0),
        json!("0"),
        json!("false "),
        json!("no\n"),
    ] {
        assert_eq!(arg_bool(&no), Some(false), "{no}");
    }
    for neither in [
        json!("maybe"),
        json!(2),
        json!(null),
        json!([true]),
        json!(""),
    ] {
        assert_eq!(arg_bool(&neither), None, "{neither}");
    }
}

/// The plugin list is builtins, sorted, apart from what this release runs natively or on the
/// controller: the 27 names in both lists, less the 11 run that way.
#[test]
fn the_plugin_list_is_sorted_builtins() {
    let implemented: Vec<&str> = ACTION_PLUGINS.iter().map(|(m, _)| *m).collect();
    let mut sorted = implemented.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, implemented, "the list is kept sorted");
    for m in &implemented {
        assert!(is_builtin(m) && !is_known(m), "{m}");
    }
    assert_eq!(implemented.len(), 16);
}

/// The page and the parity matrix read one function, so they cannot disagree about a module.
#[test]
fn every_builtin_module_has_one_route_and_the_page_prints_it() {
    let page = documentation_table();
    for m in BUILTIN_MODULES {
        let route = builtin_route(m).unwrap_or_else(|| panic!("{m} has no route"));
        let row = page.lines().find(|l| l.starts_with(&format!("| `{m}` |")));
        match route {
            Route::Statement => assert!(row.is_none(), "{m}"),
            Route::NativeOrPython => assert!(row.unwrap().contains("natively"), "{m}"),
            Route::Python => assert!(row.unwrap().ends_with("| Python module |"), "{m}"),
            Route::Agent | Route::Controller | Route::ActionPlugin => {
                assert!(row.is_some(), "{m}");
            }
        }
    }
    assert_eq!(builtin_route("community.general.ufw"), None);
    assert_eq!(
        builtin_route("ansible.builtin.uri"),
        Some(Route::ActionPlugin)
    );
}

/// Every builtin module is on the page exactly once, except the statements the compiler
/// answers: a reader asking whether `git` runs finds a row that says so.
#[test]
fn every_builtin_module_has_one_row() {
    let page = documentation_table();
    for m in BUILTIN_MODULES {
        let rows = page.matches(&format!("\n| `{m}` |")).count();
        let statement = import_module(m).is_some() || include_module(m).is_some() || *m == "meta";
        assert_eq!(rows, usize::from(!statement), "{m}");
    }
}

#[test]
fn the_documentation_table_matches_the_registry() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/src/content/docs/reference/modules.md");
    let expected = documentation_table();
    if std::env::var_os("VOLANT_UPDATE_DOCS").is_some() {
        std::fs::write(&path, &expected).unwrap();
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        actual, expected,
        "run `just docs-modules` to regenerate docs/src/content/docs/reference/modules.md"
    );
}
