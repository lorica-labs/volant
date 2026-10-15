// SPDX-License-Identifier: GPL-3.0-or-later
//! The controller interpreter a reference-dependent test runs Volant with: one with exactly the
//! recorded ansible-core.

use std::path::PathBuf;

/// The ansible-core the goldens were recorded with.
pub const RECORDED: &str = include_str!("../golden/ANSIBLE_VERSION");

/// An interpreter importing exactly the recorded ansible-core, or `None` after saying why on
/// stderr. Under `VOLANT_PYTHON` there is no skip: an interpreter that cannot reproduce the
/// recording fails the test.
pub fn reference_python() -> Option<PathBuf> {
    let want = RECORDED.trim();
    match controller_python(want) {
        Ok(python) => Some(python),
        Err(why) if std::env::var_os("VOLANT_PYTHON").is_some() => {
            panic!("VOLANT_PYTHON cannot reproduce the recording: {why}")
        }
        Err(why) => {
            eprintln!("skipped: {why}. Set VOLANT_PYTHON to an ansible-core {want}.");
            None
        }
    }
}

/// An interpreter importing exactly the ansible-core `want` names, or why there is none: the
/// payload Volant sends is built from the controller's ansible-core, so another version measures
/// the gap between versions. `VOLANT_PYTHON` is the only candidate when it is set; otherwise
/// `VIRTUAL_ENV`, the interpreter beside an `ansible-playbook` on `PATH`, then `python3`.
fn controller_python(want: &str) -> Result<PathBuf, String> {
    let candidates: Vec<PathBuf> = if let Some(explicit) = std::env::var_os("VOLANT_PYTHON") {
        vec![explicit.into()]
    } else {
        let mut candidates = Vec::new();
        if let Some(venv) = std::env::var_os("VIRTUAL_ENV") {
            candidates.push(std::path::Path::new(&venv).join("bin/python"));
        }
        let probe = std::process::Command::new("sh")
            .args(["-c", "command -v ansible-playbook"])
            .output()
            .expect("command -v runs");
        let named = String::from_utf8_lossy(&probe.stdout).trim().to_string();
        if !named.is_empty()
            && let Ok(real) = std::fs::canonicalize(&named)
            && let Some(bin) = real.parent()
        {
            candidates.push(bin.join("python3"));
            candidates.push(bin.join("python"));
        }
        candidates.push("python3".into());
        candidates
    };
    let mut seen = Vec::new();
    for python in candidates {
        let Ok(out) = std::process::Command::new(&python)
            .args([
                "-c",
                "from ansible.release import __version__; print(__version__)",
            ])
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if version == want {
            return Ok(python);
        }
        seen.push(format!("{} has ansible-core {version}", python.display()));
    }
    Err(if seen.is_empty() {
        format!("the recording is ansible-core {want} and no interpreter here can import any")
    } else {
        format!(
            "the recording is ansible-core {want}, and comparing against another version would \
             measure the version gap: {}",
            seen.join("; ")
        )
    })
}
