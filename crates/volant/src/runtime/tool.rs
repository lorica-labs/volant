// SPDX-License-Identifier: GPL-3.0-or-later
//! Running an ansible-core program through the controller's interpreter: the tool's own `main()`
//! under `python -P -c`, never its script, whose first line names the directory it was built in.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::install::python_command;
use super::{collections, resolve};

/// Starts `ansible.cli.<tool>` with `sys.argv[0]` set to the first argument: what the program
/// prints as its location exists, and its own name comes from the module, as the script's does.
const BOOTSTRAP: &str = "import sys, importlib\n\
tool = sys.argv[1]\n\
sys.argv = sys.argv[2:]\n\
importlib.import_module('ansible.cli.' + tool).main()\n";

/// Runs the ansible-core program `tool` (`doc`, `config`, ...) with `args` and returns its exit
/// code: the tool's own, `128 + signal` when a signal ended it, 1 when it could not start.
pub fn run_tool(tool: &str, args: &[OsString]) -> i32 {
    match start(tool, args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("[ERROR]: {message}");
            1
        }
    }
}

fn start(tool: &str, args: &[OsString]) -> Result<i32, String> {
    let config = crate::config::Config::load().map_err(|e| format!("{e:#}"))?;
    resolve::configured(config);
    let explicit = std::env::var("VOLANT_PYTHON").ok();
    let virtual_env = std::env::var("VIRTUAL_ENV").ok();
    let python = crate::python::find_python(explicit.as_deref(), virtual_env.as_deref(), true)
        .map_err(|e| format!("{e:#}"))?;
    let mut command = command(&python, explicit.is_some(), tool, &started_as(), args);
    if let Some(paths) = collections::helper_env() {
        command.env("ANSIBLE_COLLECTIONS_PATH", paths);
    }
    run(&mut command, &python)
}

/// The command line: the interpreter without the working directory on its module path. Under
/// the managed runtime, the environment of the runtime's other children plus the user's own, bar
/// the variables that change what the interpreter loads; under `VOLANT_PYTHON`, the user's whole
/// environment, as the interpreter is theirs.
fn command(
    python: &str,
    user_python: bool,
    tool: &str,
    argv0: &Path,
    args: &[OsString],
) -> Command {
    let mut command = if user_python {
        Command::new(python)
    } else {
        let mut command = python_command(Path::new(python));
        // Nothing but the terminal, the pager and the `ANSIBLE_*` settings is what the tool reads.
        for (name, value) in std::env::vars_os() {
            if forwarded(&name) {
                command.env(name, value);
            }
        }
        command.stdin(Stdio::inherit());
        command
    };
    command
        .arg("-P")
        .args(["-c", BOOTSTRAP])
        .arg(tool)
        .arg(argv0)
        .args(args);
    command
}

/// Whether a variable of the user's reaches a tool run under the managed runtime: everything but
/// what changes which Python code loads (`PYTHON*`) or how pip installs (`PIP_*`).
fn forwarded(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    !name.starts_with("PYTHON") && !name.starts_with("PIP_")
}

/// The path this program was started as, made to name a file that exists: a bare name was found
/// on `PATH`, so it is looked up there; `current_exe` when that fails.
fn started_as() -> PathBuf {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    let path = PathBuf::from(&argv0);
    if path.components().count() > 1 || argv0.is_empty() {
        return absolute(path);
    }
    let on_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(&argv0))
            .find(|candidate| candidate.is_file())
    });
    on_path
        .map(absolute)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or(path)
}

fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

#[cfg(unix)]
fn run(command: &mut Command, python: &str) -> Result<i32, String> {
    use std::os::unix::process::ExitStatusExt as _;

    extern "C" fn ignore(_: libc::c_int) {}
    // The terminal sends ^C and ^\ to the whole group: the tool handles them, and this process
    // waits for it. A handler, not SIG_IGN, because an ignored signal stays ignored in the child.
    // SAFETY: `ignore` does nothing, so it is async-signal-safe.
    unsafe {
        let handler = ignore as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGQUIT, handler);
    }
    let status = command
        .status()
        .map_err(|e| format!("cannot run {python}: {e}"))?;
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1))
}

#[cfg(not(unix))]
fn run(command: &mut Command, python: &str) -> Result<i32, String> {
    let status = command
        .status()
        .map_err(|e| format!("cannot run {python}: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests;
