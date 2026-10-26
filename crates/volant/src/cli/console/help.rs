// SPDX-License-Identifier: GPL-3.0-or-later
//! `help`: `cmd.Cmd.do_help` and its `columnize`, the console's `help_*` texts, and a module's
//! documentation read from the managed runtime's ansible-core.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

use super::session::Session;

/// The commands with a `do_` method and a help text, beside the modules.
const BUILTINS: [&str; 17] = [
    "EOF",
    "become",
    "become_method",
    "become_user",
    "cd",
    "check",
    "collections",
    "diff",
    "exit",
    "forks",
    "help",
    "list",
    "remote_user",
    "serial",
    "shell",
    "timeout",
    "verbosity",
];

/// Reads a module's documentation as `helpdefault` does: its short description, then the first
/// line of each option's description, as JSON, or `null` when it has none.
const DOC: &str = r#"
import json, sys
from ansible.plugins.loader import init_plugin_loader, module_loader, fragment_loader
from ansible.utils import plugin_docs
init_plugin_loader([])
path = module_loader.find_plugin(sys.argv[1])
oc = plugin_docs.get_docstring(path, fragment_loader)[0] if path else None
print(json.dumps(oc and {"s": oc["short_description"],
    "o": [[k, v["description"][0]] for k, v in oc["options"].items()]}))
"#;

/// Every command name Tab completes first: the console's own and the modules'.
pub(super) fn command_names(session: &Session) -> Vec<String> {
    let mut names: Vec<String> = BUILTINS.iter().map(|b| (*b).to_string()).collect();
    names.extend(session.modules());
    names.sort();
    names.dedup();
    names
}

impl Session {
    pub(super) fn help(&self, arg: &str) {
        if arg.is_empty() {
            let names = command_names(self);
            let heading = "Documented commands (type help <topic>):";
            println!();
            println!("{heading}");
            println!("{}", "=".repeat(heading.len()));
            print!("{}", columnize(&names, 79));
            println!();
            return;
        }
        let text: &[&str] = match arg {
            "shell" => &["You can run shell commands through the shell module."],
            "forks" | "serial" => &[
                "Set the number of forks to use per task",
                "Usage: forks <number>",
            ],
            "collections" => &[
                "Set the collection name search path when using short names for plugins",
                "Usage: collections <collection1>[, <collection2> ...]\n Use empty quotes or \"none\" to reset to default.\n",
            ],
            "verbosity" => &["Set the verbosity level, equivalent to -v for 1 and -vvvv for 4."],
            "cd" => &[
                "Change active host/group. ",
                "Usage: cd <group>|<host>|<host pattern>",
            ],
            "list" => {
                &["List the hosts in the current group or a list of groups if you add 'groups'."]
            }
            "become" => &["Toggle whether the tasks are run with become"],
            "remote_user" => &["Set the user for use as login to the remote target"],
            "become_user" => &[
                "Set the user for use with privilege escalation (which remote user attempts to 'become' when become is enabled)",
            ],
            "become_method" => {
                &["Set the privilege escalation plugin to use when become is enabled"]
            }
            "check" => &["Toggle check_mode for the tasks"],
            "diff" => &["Toggle diff output for the tasks"],
            "timeout" => &["Set task timeout in seconds", "Usage: timeout <seconds>"],
            "exit" | "EOF" => &["LEAVE!"],
            "help" => {
                self.warn("Don't be redundant!");
                return;
            }
            _ => &[],
        };
        if !text.is_empty() {
            for line in text {
                println!("{line}");
            }
        } else if self.modules().iter().any(|m| m == arg) {
            self.module_help(arg);
        } else {
            println!("*** No help on {arg}");
        }
    }

    /// `helpdefault`: the module's short description and options, from the runtime's
    /// ansible-core, which holds the documentation.
    fn module_help(&self, name: &str) {
        let Some(doc) = module_doc(name) else {
            eprintln!("[ERROR]: No documentation found for {name}.");
            return;
        };
        println!("{}", doc.0);
        println!("Parameters:");
        for (option, first) in doc.1 {
            let shown = if self.colored() {
                format!("\x1b[1;37m{option}\x1b[0m")
            } else {
                option
            };
            println!("  {shown} {first}");
        }
    }

    /// Every module the session knows by its full name: the builtins of ansible-core, and the
    /// modules of the collections installed where the configuration looks for them.
    pub(super) fn modules(&self) -> Vec<String> {
        let mut names: Vec<String> = volant_protocol::modules::BUILTIN_MODULES
            .iter()
            .map(|m| format!("ansible.builtin.{m}"))
            .collect();
        for root in self.collection_roots() {
            names.extend(collection_modules(&root.join("ansible_collections")));
        }
        names.sort();
        names.dedup();
        names
    }
}

/// `namespace.collection.module` for each module file under `root`.
fn collection_modules(root: &std::path::Path) -> Vec<String> {
    let dirs = |dir: &std::path::Path| -> Vec<(String, PathBuf)> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect()
    };
    let mut names = Vec::new();
    for (namespace, ns_dir) in dirs(root) {
        for (collection, dir) in dirs(&ns_dir) {
            let modules = dir.join("plugins").join("modules");
            for entry in std::fs::read_dir(modules).into_iter().flatten().flatten() {
                let file = entry.file_name().to_string_lossy().into_owned();
                if let Some(module) = file.strip_suffix(".py")
                    && !module.starts_with('_')
                {
                    names.push(format!("{namespace}.{collection}.{module}"));
                }
            }
        }
    }
    names
}

/// The documentation of `name` from the runtime's ansible-core, or `None` without a runtime or
/// without documentation.
fn module_doc(name: &str) -> Option<(String, Vec<(String, String)>)> {
    let python = crate::runtime::resolve::controller_python()?;
    let output = Command::new(python)
        .args(["-c", DOC, name])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let doc: Value = serde_json::from_slice(&output.stdout).ok()?;
    let text = |v: &Value| v.as_str().map_or_else(|| v.to_string(), str::to_string);
    let options = doc
        .get("o")?
        .as_array()?
        .iter()
        .filter_map(|pair| Some((text(pair.get(0)?), text(pair.get(1)?))))
        .collect();
    Some((text(doc.get("s")?), options))
}

/// `cmd.Cmd.columnize`: as few rows as let the columns fit in `width`, filled column by column,
/// each column as wide as its longest name, two spaces between columns.
pub(super) fn columnize(names: &[String], width: usize) -> String {
    let size = names.len();
    if size == 0 {
        return "<empty>\n".into();
    }
    if size == 1 {
        return format!("{}\n", names[0]);
    }
    let mut layout = (size, 1, vec![0]);
    for rows in 1..size {
        let cols = size.div_ceil(rows);
        let mut widths = Vec::new();
        let mut total: isize = -2;
        for col in 0..cols {
            let widest = (0..rows)
                .map(|row| row + rows * col)
                .take_while(|i| *i < size)
                .map(|i| names[i].chars().count())
                .max()
                .unwrap_or(0);
            widths.push(widest);
            total += isize::try_from(widest).unwrap_or(isize::MAX) + 2;
            if total > isize::try_from(width).unwrap_or(isize::MAX) {
                break;
            }
        }
        if total <= isize::try_from(width).unwrap_or(isize::MAX) {
            layout = (rows, cols, widths);
            break;
        }
    }
    let (rows, cols, widths) = layout;
    let mut out = String::new();
    for row in 0..rows {
        let mut texts: Vec<&str> = (0..cols)
            .map(|col| names.get(row + rows * col).map_or("", String::as_str))
            .collect();
        while texts.last().is_some_and(|t| t.is_empty()) {
            texts.pop();
        }
        let padded: Vec<String> = texts
            .iter()
            .enumerate()
            .map(|(col, t)| format!("{t:<width$}", width = widths[col]))
            .collect();
        out.push_str(&padded.join("  "));
        out.push('\n');
    }
    out
}
