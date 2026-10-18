// SPDX-License-Identifier: GPL-3.0-or-later
//! Which Python the controller runs ansible-core under: the one order the helper, the union cache
//! key and `volant runtime status` all read.
//!
//! 1. `VOLANT_PYTHON`, alone when it is set, whatever the mode: it names the interpreter;
//! 2. Volant's runtime, when it is installed (not under `system`);
//! 3. `$VIRTUAL_ENV/bin/python`, then 4. `python3` from `PATH` (not under `managed`);
//! 5. when none of them has ansible-core, Volant's runtime installed there and then (not under
//!    `system`, and never for a run that only gathers facts).

use std::fmt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use super::install::{self, Installed};
use super::manifest::Manifest;
use super::runtimes_dir;

/// `[volant] runtime`, or `VOLANT_RUNTIME`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Volant's runtime when it is installed, else the system's Python, else Volant's runtime
    /// installed on first need.
    #[default]
    Auto,
    /// Volant's runtime only, installed on first need.
    Managed,
    /// The system's Python only; nothing is installed.
    System,
}

impl Mode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Managed => "managed",
            Mode::System => "system",
        }
    }

    pub(crate) fn parse(word: &str) -> Option<Mode> {
        match word {
            "auto" => Some(Mode::Auto),
            "managed" => Some(Mode::Managed),
            "system" => Some(Mode::System),
            _ => None,
        }
    }
}

/// Why an interpreter is a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Explicit,
    Managed,
    VirtualEnv,
    Path,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Origin::Explicit => "VOLANT_PYTHON",
            Origin::Managed => "Volant's runtime",
            Origin::VirtualEnv => "VIRTUAL_ENV",
            Origin::Path => "python3 on PATH",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) python: String,
    pub(crate) origin: Origin,
}

static MODE: OnceLock<Mode> = OnceLock::new();

/// Sets the mode for the rest of the process from the configuration a run has just read.
pub(crate) fn configured(config: crate::config::Config) -> crate::config::Config {
    _ = MODE.set(config.runtime);
    config
}

/// The mode the run set. Before that, or in a process that never reads a configuration (the
/// library's own tests), `system`: nothing is installed behind a caller's back.
pub(crate) fn mode() -> Mode {
    MODE.get().copied().unwrap_or(Mode::System)
}

/// The interpreters to try, in order, without running anything. `managed` is Volant's runtime's
/// interpreter when it is installed.
pub(crate) fn candidates(
    mode: Mode,
    explicit: Option<&str>,
    virtual_env: Option<&str>,
    managed: Option<PathBuf>,
) -> Vec<Candidate> {
    let candidate = |python: String, origin| Candidate { python, origin };
    if let Some(python) = explicit {
        return vec![candidate(python.to_owned(), Origin::Explicit)];
    }
    let mut out = Vec::new();
    if mode != Mode::System
        && let Some(python) = managed
    {
        out.push(candidate(python.display().to_string(), Origin::Managed));
    }
    if mode != Mode::Managed {
        if let Some(env) = virtual_env {
            out.push(candidate(format!("{env}/bin/python"), Origin::VirtualEnv));
        }
        out.push(candidate("python3".to_owned(), Origin::Path));
    }
    out
}

/// This process's candidates: the environment, the mode, and Volant's runtime when it is
/// installed, which costs two `stat`s once the manifest has been read.
pub(crate) fn current(explicit: Option<&str>, virtual_env: Option<&str>) -> Vec<Candidate> {
    let mode = mode();
    let managed = (explicit.is_none() && mode != Mode::System)
        .then(managed)
        .flatten()
        .map(|done| done.python);
    candidates(mode, explicit, virtual_env, managed)
}

/// Whether `python` imports ansible-core. The probe is the import itself rather than a version
/// string: an interpreter that cannot import `ansible` cannot build a payload, whatever it reports.
pub(crate) fn has_ansible(python: &str) -> bool {
    Command::new(python)
        .args(["-c", "import ansible"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// What `volant runtime status` says about the resolution: the mode, the interpreter the next
/// run would use and why, and every candidate in order.
pub(crate) fn describe() -> Vec<String> {
    let explicit = std::env::var("VOLANT_PYTHON").ok();
    let virtual_env = std::env::var("VIRTUAL_ENV").ok();
    let mode = mode();
    let list = current(explicit.as_deref(), virtual_env.as_deref());
    let next = match list.iter().find(|candidate| has_ansible(&candidate.python)) {
        Some(found) => format!("{} ({})", found.python, found.origin),
        None if may_install(mode, explicit.as_deref()) => {
            "Volant's runtime, installed when the run needs it".to_owned()
        }
        None => "nothing: no candidate has ansible-core".to_owned(),
    };
    let mut lines = vec![
        format!("runtime mode: {}", mode.name()),
        format!("next run uses: {next}"),
    ];
    lines.extend(
        list.iter()
            .map(|candidate| format!("  candidate: {} ({})", candidate.python, candidate.origin)),
    );
    lines
}

/// The pinned manifest, read once per process.
fn pinned() -> Result<&'static Manifest, String> {
    static PINNED: OnceLock<Result<Manifest, String>> = OnceLock::new();
    PINNED
        .get_or_init(Manifest::pinned)
        .as_ref()
        .map_err(Clone::clone)
}

/// Volant's runtime, when it is installed.
pub(crate) fn managed() -> Option<Installed> {
    install::installed(&runtimes_dir().ok()?, &pinned().ok()?.id())
}

/// Whether a run may install the runtime when no candidate has ansible-core.
pub(crate) fn may_install(mode: Mode, explicit: Option<&str>) -> bool {
    explicit.is_none() && mode != Mode::System
}

/// Installs Volant's runtime and returns its interpreter. A runtime already in place that has
/// lost its ansible-core (a file deleted by hand, a disk error) is refused rather than handed back.
pub(crate) fn install_now() -> Result<String, String> {
    let done = install::install(&runtimes_dir()?, pinned()?)?;
    let python = done.python.display().to_string();
    if !has_ansible(&python) {
        let id = done.id;
        return Err(format!(
            "Volant's runtime {id} is installed but cannot import ansible-core: run \
             `volant runtime remove {id}`, then `volant runtime install`"
        ));
    }
    Ok(python)
}

/// What the refusal adds when no candidate has ansible-core: the way to Volant's own runtime
/// the mode leaves open. Nothing under `VOLANT_PYTHON`, which wins over the runtime.
pub(crate) fn hint(mode: Mode, explicit: bool) -> &'static str {
    match mode {
        _ if explicit => "",
        Mode::System => {
            ". Volant can also install its own with `volant runtime install`, used when \
             `[volant] runtime` is not `system`"
        }
        Mode::Auto | Mode::Managed => ", or run `volant runtime install`",
    }
}
