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
        Err(err) => {
            // The words and code of a failed `volant playbook` setup.
            eprintln!("ERROR! {err:#}");
            crate::stats::error_code(&err)
        }
    }
}

fn start(tool: &str, args: &[OsString]) -> anyhow::Result<i32> {
    let config = crate::config::Config::load()?;
    resolve::configured(config);
    let explicit = std::env::var("VOLANT_PYTHON").ok();
    let virtual_env = std::env::var("VIRTUAL_ENV").ok();
    let python = crate::python::find_python(explicit.as_deref(), virtual_env.as_deref(), true)?;
    // Only Volant's own runtime is cleaned up; any other interpreter is the user's and is
    // started as the playbook helper starts it.
    let managed =
        resolve::managed().is_some_and(|done| done.python.display().to_string() == python);
    // `ansible-pull` starts `ansible` and `ansible-playbook` beside its own path: Volant's links.
    // Asking for help or the version starts nothing, so it needs no links.
    let asks_only = args
        .iter()
        .any(|a| ["-h", "--help", "--version"].iter().any(|w| a == w));
    let argv0 = if tool == "pull" && !asks_only {
        super::pull_links::dir()
            .map_err(anyhow::Error::msg)?
            .join("ansible-pull")
    } else {
        started_as()
    };
    let mut command = command(&python, managed, tool, &argv0, args);
    if let Some(paths) = collections::helper_env() {
        command.env("ANSIBLE_COLLECTIONS_PATH", paths);
    }
    run(&mut command, &python).map_err(anyhow::Error::msg)
}

/// The command line: the interpreter without the working directory on its module path. Under
/// the managed runtime, the environment of the runtime's other children plus the user's own, bar
/// the variables that change what the interpreter loads; any other interpreter keeps the user's
/// whole environment, as the interpreter is theirs.
fn command(python: &str, managed: bool, tool: &str, argv0: &Path, args: &[OsString]) -> Command {
    let mut command = if managed {
        let mut command = python_command(Path::new(python));
        // Nothing but the terminal, the pager and the `ANSIBLE_*` settings is what the tool reads.
        for (name, value) in std::env::vars_os() {
            if forwarded(&name) {
                command.env(name, value);
            }
        }
        command.stdin(Stdio::inherit());
        command
    } else {
        Command::new(python)
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

/// The tool's pid while it runs, for the signals this process forwards.
#[cfg(unix)]
static CHILD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn ignore(_: libc::c_int) {}

#[cfg(unix)]
extern "C" fn forward(signal: libc::c_int) {
    let pid = CHILD.load(std::sync::atomic::Ordering::Relaxed);
    if pid > 0 {
        // SAFETY: `kill` is async-signal-safe.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

/// Sets `handler` for `signal` unless it was started ignored (`nohup`, a background job), in
/// which case it stays ignored and the tool inherits that. A handler, never `SIG_IGN`, because
/// an ignored signal stays ignored in the child.
#[cfg(unix)]
fn handle(signal: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    // SAFETY: the handlers only call `kill` or do nothing, so they are async-signal-safe.
    unsafe {
        let before = libc::signal(signal, libc::SIG_IGN);
        if before != libc::SIG_IGN {
            libc::signal(signal, handler as *const () as libc::sighandler_t);
        }
    }
}

#[cfg(unix)]
fn run(command: &mut Command, python: &str) -> Result<i32, String> {
    use std::os::unix::process::ExitStatusExt as _;

    // The terminal sends ^C and ^\ to the whole group: the tool handles them and this process
    // waits for it. SIGTERM and SIGHUP reach only this process, so they go on to the tool.
    handle(libc::SIGINT, ignore);
    handle(libc::SIGQUIT, ignore);
    handle(libc::SIGTERM, forward);
    handle(libc::SIGHUP, forward);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run {python}: {e}"))?;
    CHILD.store(
        i32::try_from(child.id()).unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );
    let status = child
        .wait()
        .map_err(|e| format!("cannot wait for {python}: {e}"))?;
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
