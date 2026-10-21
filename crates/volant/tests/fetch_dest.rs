// SPDX-License-Identifier: GPL-3.0-or-later
//! `[volant] host_named_dest_dirs` and `host_named_paths`: where a `fetch` may write when a managed
//! host named its `dest`, through a registered result or a fact. Without either, it writes it.
//!
//! The host here is the controller (`ansible_connection=local`), so a host-named path is the
//! output of an `echo`. The tests skip when the recorded ansible-core is not there, as the
//! scenarios do.

#![cfg(unix)]

use std::fmt::Write as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rstest::rstest;

#[path = "common/process.rs"]
mod process;

#[path = "common/python.rs"]
mod python;

const DEADLINE: Duration = Duration::from_secs(120);

/// What the file of a refusal says, for a `dest` that no directory covers.
const NEVER: &str = "the 'dest' of this task was named by a managed host, and a controller path a host chose is never written unless host_named_dest_dirs names a directory above it";

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// The directory the setting names.
    allowed: PathBuf,
    /// A directory beside it, which a host must never write to.
    outside: PathBuf,
    home: PathBuf,
    play: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fx = Self {
            allowed: root.join("allowed"),
            outside: root.join("outside"),
            home: root.join("home"),
            play: root.join("play"),
            root,
            _dir: dir,
        };
        for path in [&fx.allowed, &fx.outside, &fx.home, &fx.play] {
            std::fs::create_dir_all(path).unwrap();
        }
        // Private whatever the umask: a group-writable directory of the list holds nothing.
        std::fs::set_permissions(&fx.allowed, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&fx.outside, fx.allowed.join("link")).unwrap();
        std::fs::write(fx.root.join("host.txt"), "fetched\n").unwrap();
        std::fs::write(fx.play.join("inv.ini"), "h1 ansible_connection=local\n").unwrap();
        fx
    }

    /// The tasks of one run: each `dest` is the output of an `echo`, so a host named it.
    fn write(&self, dests: &[(&str, String)], config: &str) {
        let mut tasks = String::new();
        for (name, dest) in dests {
            writeln!(
                tasks,
                "    - name: \"{name}/name\"\n      command: echo {dest}\n      args: {{expand_argument_vars: false}}\n      register: r\n      changed_when: false\n    - name: \"{name}\"\n      fetch: {{src: \"{src}\", dest: \"{{{{ r.stdout }}}}\", flat: true}}\n      ignore_errors: true\n",
                src = self.root.join("host.txt").display()
            )
            .unwrap();
        }
        std::fs::write(
            self.play.join("site.yml"),
            format!("- hosts: h1\n  gather_facts: false\n  tasks:\n{tasks}"),
        )
        .unwrap();
        std::fs::write(self.play.join("ansible.cfg"), config).unwrap();
    }

    fn run(&self, python: &Path) -> String {
        self.run_with(python, &[])
    }

    /// [`Self::run`] with `env` set on top of the clean environment.
    fn run_with(&self, python: &Path, env: &[(&str, &str)]) -> String {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_volant"));
        command
            .args(["playbook", "-i", "inv.ini", "site.yml"])
            .current_dir(&self.play);
        clean_environment(&mut command);
        command
            .env("HOME", &self.home)
            .env("VOLANT_PYTHON", python)
            .env("ANSIBLE_CONFIG", self.play.join("ansible.cfg"))
            .envs(env.iter().copied());
        let out = process::run_within(&mut command, DEADLINE, None);
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }
}

fn clean_environment(command: &mut std::process::Command) {
    for (name, _) in std::env::vars() {
        if name.starts_with("ANSIBLE_") || name.starts_with("VOLANT_") {
            command.env_remove(name);
        }
    }
    command.env("NO_COLOR", "1").env("VOLANT_RUNTIME", "system");
}

/// The block of the output that belongs to the task `name`.
fn block<'a>(stdout: &'a str, name: &str) -> &'a str {
    let head = format!("TASK [{name}]");
    let from = stdout
        .find(&head)
        .unwrap_or_else(|| panic!("{name}:\n{stdout}"));
    let rest = &stdout[from + head.len()..];
    &rest[..rest.find("\nTASK [").unwrap_or(rest.len())]
}

/// Every file below `dir`, for a test that says nothing was written.
fn files_below(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            found.extend(files_below(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Only a path that lies strictly inside the directory of the setting is written; every other
/// spelling is refused, and nothing is created where it points.
#[test]
fn a_dest_a_host_named_is_written_only_inside_the_directories() {
    let Some(python) = python::reference_python() else {
        return;
    };
    let fx = Fixture::new();
    let allowed = fx.allowed.display();
    let rows = [
        ("inside", format!("{allowed}/got.txt")),
        ("nested", format!("{allowed}/new/dir/got.txt")),
        ("outside", format!("{}/got.txt", fx.outside.display())),
        ("dotdot", format!("{allowed}/../outside/got.txt")),
        ("link", format!("{allowed}/link/got.txt")),
        ("root", allowed.to_string()),
        ("sibling", format!("{allowed}-evil/got.txt")),
        ("relative", "got.txt".to_string()),
        ("tilde", "~/got.txt".to_string()),
    ];
    fx.write(
        &rows,
        &format!("[volant]\nhost_named_dest_dirs = {allowed}\n"),
    );
    let stdout = fx.run(&python);
    for name in ["inside", "nested"] {
        assert!(
            block(&stdout, name).contains("\nchanged: [h1]"),
            "{name}:\n{stdout}"
        );
    }
    for (name, dest) in &rows[2..] {
        let said = block(&stdout, name);
        assert!(
            said.contains(&format!(
                "the 'dest' of this task was named by a managed host, and '{dest}' "
            )) && said.contains(&format!("(host_named_dest_dirs = {allowed})")),
            "{name}:\n{stdout}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(fx.allowed.join("got.txt")).unwrap(),
        "fetched\n"
    );
    assert!(fx.allowed.join("new/dir/got.txt").is_file());
    assert_eq!(files_below(&fx.outside), Vec::<PathBuf>::new(), "{stdout}");
    // The `~` and the relative `dest` landed nowhere. The home directory also holds the cache
    // of the run, so the files are looked for by name.
    for place in [&fx.home, &fx.play] {
        assert!(
            !place.join("got.txt").exists(),
            "{}:\n{stdout}",
            place.display()
        );
    }
    assert!(!fx.root.join("allowed-evil").exists(), "{stdout}");
}

/// A host-named `dest` is written by default, as ansible-core writes it; an empty list,
/// `contained` and `refuse`, from the file or the environment, refuse it and name the setting.
#[rstest]
#[case::default("", &[], true)]
#[case::allow("[volant]\nhost_named_paths = allow\n", &[], true)]
#[case::empty_list("[volant]\nhost_named_dest_dirs =\n", &[], false)]
#[case::contained("[volant]\nhost_named_paths = contained\n", &[], false)]
#[case::refuse("[volant]\nhost_named_paths = refuse\n", &[], false)]
#[case::refuse_env("", &[("VOLANT_HOST_NAMED_PATHS", "refuse")], false)]
#[case::empty_list_env("", &[("VOLANT_HOST_NAMED_DEST_DIRS", "")], false)]
fn a_dest_a_host_named_is_written_unless_the_protection_is_on(
    #[case] config: &str,
    #[case] env: &[(&str, &str)],
    #[case] written: bool,
) {
    let Some(python) = python::reference_python() else {
        return;
    };
    let fx = Fixture::new();
    let marker = fx.allowed.join("marker.txt");
    fx.write(&[("marker", marker.display().to_string())], config);
    let stdout = fx.run_with(&python, env);
    let said = block(&stdout, "marker");
    if written {
        assert!(said.contains("\nchanged: [h1]"), "{stdout}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "fetched\n");
    } else {
        assert!(said.contains(NEVER), "{stdout}");
        assert!(
            !marker.exists(),
            "a host-chosen dest was written:\n{stdout}"
        );
    }
}

/// A list that names another directory refuses a `dest` outside it, whatever `host_named_paths`
/// says, and names the list in the message.
#[rstest]
#[case::allow("allow")]
#[case::contained("contained")]
fn a_dest_a_host_named_outside_the_list_is_refused(#[case] mode: &str) {
    let Some(python) = python::reference_python() else {
        return;
    };
    let fx = Fixture::new();
    let marker = fx.allowed.join("marker.txt");
    fx.write(
        &[("marker", marker.display().to_string())],
        &format!(
            "[volant]\nhost_named_paths = {mode}\nhost_named_dest_dirs = /nonexistent/volant-dest\n"
        ),
    );
    let stdout = fx.run(&python);
    assert!(
        block(&stdout, "marker").contains("(host_named_dest_dirs = /nonexistent/volant-dest)"),
        "{stdout}"
    );
    assert!(!marker.exists(), "{stdout}");
}

/// A directory that other users can write is no place for a host's choice, a sticky one such as
/// `/tmp` is.
#[rstest]
#[case::world_writable(0o777, false)]
#[case::group_writable(0o775, false)]
#[case::sticky(0o1777, true)]
fn a_directory_other_users_write_holds_a_dest_only_when_it_is_sticky(
    #[case] mode: u32,
    #[case] written: bool,
) {
    let Some(python) = python::reference_python() else {
        return;
    };
    let fx = Fixture::new();
    std::fs::set_permissions(&fx.allowed, std::fs::Permissions::from_mode(mode)).unwrap();
    let dest = fx.allowed.join("got.txt");
    fx.write(
        &[("shared", dest.display().to_string())],
        &format!(
            "[volant]\nhost_named_dest_dirs = {}\n",
            fx.allowed.display()
        ),
    );
    let stdout = fx.run(&python);
    assert_eq!(dest.exists(), written, "{stdout}");
    assert_eq!(
        block(&stdout, "shared").contains("\nchanged: [h1]"),
        written,
        "{stdout}"
    );
}

/// A value that is not a colon-separated list of absolute directories stops the run before it
/// starts, from the file and from the environment, naming the key, the value and the entry.
#[rstest]
#[case::relative("relative/dir", "relative/dir", "is not an absolute path")]
#[case::tilde("~/fetch", "~/fetch", "is not an absolute path")]
#[case::dotdot("/srv/a/../b", "/srv/a/../b", "has a '..' component")]
#[case::empty_entry("/srv/a::/srv/b", "", "is empty")]
#[case::root("/", "/", "is the root directory")]
fn a_list_that_is_not_absolute_directories_stops_the_run(
    #[case] value: &str,
    #[case] entry: &str,
    #[case] why: &str,
    #[values(false, true)] from_env: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let play = dir.path().join("play.yml");
    std::fs::write(
        &play,
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - debug: {msg: started}\n",
    )
    .unwrap();
    let config = dir.path().join("ansible.cfg");
    let (file_value, origin) = if from_env {
        ("/srv/ok", "env: VOLANT_HOST_NAMED_DEST_DIRS".to_string())
    } else {
        (value, config.display().to_string())
    };
    std::fs::write(
        &config,
        format!("[volant]\nhost_named_dest_dirs = {file_value}\n"),
    )
    .unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_volant"));
    command.arg("playbook").arg(&play).current_dir(dir.path());
    clean_environment(&mut command);
    command.env("ANSIBLE_CONFIG", &config);
    if from_env {
        command.env("VOLANT_HOST_NAMED_DEST_DIRS", value);
    }
    let out = process::run_within(&mut command, DEADLINE, None);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.status.code(), Some(5), "{stdout}{stderr}");
    assert!(
        stderr.contains(&format!(
            "Config 'VOLANT_HOST_NAMED_DEST_DIRS' from '{origin}' has an invalid value: '{value}'; expected a colon-separated list of absolute directories (entry '{entry}' {why})"
        )),
        "{stderr}"
    );
    assert!(!stdout.contains("PLAY ["), "the run started:\n{stdout}");
}
