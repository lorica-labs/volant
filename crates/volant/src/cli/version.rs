// SPDX-License-Identifier: GPL-3.0-or-later
//! What `--version` prints for an ansible-core program, and what `-vv` opens an ad hoc run with:
//! the reference's block of the release and the paths the program works with.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::announce::{config_path, py_str};

/// Run by the controller's interpreter: the four lines about the Python side, in the reference's
/// words, or nothing when `ansible` cannot be imported.
const PROBE: &str = r#"
import os, sys
import ansible, jinja2, yaml
try:
    from yaml import _yaml
    libyaml = " (with libyaml v%s)" % _yaml.get_version_string()
except ImportError:
    libyaml = " (without libyaml)"
print(os.path.dirname(ansible.__file__))
print("".join(sys.version.splitlines()) + " (" + sys.executable + ")")
print(jinja2.__version__)
print(yaml.__version__ + libyaml)
"#;

/// The block for `program`, without its final newline. `modules` are the `-M` directories, put
/// in front of the configured search path.
///
/// The lines about the Python side come from the interpreter Volant would use and are left out
/// when it has none: nothing is installed to print a version. The search paths read the
/// environment and the defaults, and not `library` or `collections_path` of `ansible.cfg`.
pub(super) fn block(program: &str, modules: &[PathBuf]) -> String {
    let home = std::env::var("ANSIBLE_HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.ansible")))
        .unwrap_or_else(|| "~/.ansible".to_string());
    let listed = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| v.split(':').map(str::to_string).collect::<Vec<_>>())
    };
    let mut search: Vec<String> = modules.iter().map(|d| d.display().to_string()).collect();
    search.extend(listed("ANSIBLE_LIBRARY").unwrap_or_else(|| {
        vec![
            format!("{home}/plugins/modules"),
            "/usr/share/ansible/plugins/modules".to_string(),
        ]
    }));
    let collections = listed("ANSIBLE_COLLECTIONS_PATH")
        .or_else(|| listed("ANSIBLE_COLLECTIONS_PATHS"))
        .unwrap_or_else(|| {
            vec![
                format!("{home}/collections"),
                "/usr/share/ansible/collections".to_string(),
            ]
        });
    let search = search
        .iter()
        .map(|d| py_str(d))
        .collect::<Vec<_>>()
        .join(", ");
    let config = config_path().map_or("None".to_string(), |p| p.display().to_string());
    let release = crate::vars::ansible_version()["full"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let python = python_lines();
    let mut lines = vec![
        format!("{program} [core {release}]"),
        format!("  config file = {config}"),
        format!("  configured module search path = [{search}]"),
    ];
    if let Some(python) = &python {
        lines.push(format!("  ansible python module location = {}", python[0]));
    }
    lines.push(format!(
        "  ansible collection location = {}",
        collections.join(":")
    ));
    lines.push(format!(
        "  executable location = {}",
        executable().display()
    ));
    if let Some(python) = &python {
        lines.push(format!("  python version = {}", python[1]));
        lines.push(format!("  jinja version = {}", python[2]));
        lines.push(format!("  pyyaml version = {}", python[3]));
    }
    lines.join("\n")
}

/// The four lines of [`PROBE`], when the controller's interpreter answers it.
fn python_lines() -> Option<Vec<String>> {
    let python = crate::runtime::resolve::controller_python()?;
    let output = Command::new(python)
        .args(["-c", PROBE])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    (lines.len() == 4).then_some(lines)
}

/// The program that was started: `argv[0]` made absolute, looked up on `PATH` when it has no
/// directory, as the shell did.
fn executable() -> PathBuf {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    let path = Path::new(&argv0);
    if path.components().count() == 1
        && let Some(found) = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .map(|dir| dir.join(path))
            .find(|candidate| candidate.is_file())
    {
        return std::path::absolute(&found).unwrap_or(found);
    }
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}
