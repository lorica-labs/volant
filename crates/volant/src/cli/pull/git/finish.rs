// SPDX-License-Identifier: GPL-3.0-or-later
//! What follows the checkout: the signature check and the submodules.

use std::path::Path;

use super::{Failure, Git, REMOTE, failure};

impl Git {
    /// `verify_commit_sign`: `verify-tag` for an annotated tag, `verify-commit` otherwise.
    pub(super) fn verify(&self, dest: &Path, version: &str) -> Result<(), Failure> {
        let tags = self.run(
            &[
                "for-each-ref",
                "refs/tags/",
                "--format",
                "%(objecttype):%(refname:short)",
            ],
            Some(dest),
        );
        if tags.rc != 0 {
            return Err(failure(&[
                (
                    "msg",
                    format!("Could not determine tag data - received {}", tags.out).into(),
                ),
                ("stderr", tags.err.into()),
                ("stdout", tags.out.into()),
            ]));
        }
        let annotated = tags
            .out
            .lines()
            .filter_map(|l| l.trim().split_once(':'))
            .any(|(kind, name)| kind == "tag" && name == version);
        let sub = if annotated {
            "verify-tag"
        } else {
            "verify-commit"
        };
        let ran = self.run(&[sub, version], Some(dest));
        if ran.rc != 0 {
            let msg = format!("Failed to verify GPG signature of commit/tag \"{version}\"");
            return Err(failure(&[
                ("msg", msg.into()),
                ("rc", ran.rc.into()),
                ("stderr", ran.err.into()),
                ("stdout", ran.out.into()),
            ]));
        }
        Ok(())
    }

    pub(super) fn submodule_hashes(
        &self,
        dest: &Path,
        version: &str,
    ) -> Result<Vec<(String, String)>, Failure> {
        let git = self.path.display().to_string();
        let ran = self.run(
            &["submodule", "foreach", &git, "rev-parse", version],
            Some(dest),
        );
        if ran.rc != 0 {
            return Err(failure(&[
                ("msg", "Unable to determine hashes of submodules".into()),
                ("rc", ran.rc.into()),
                ("stderr", ran.err.into()),
                ("stdout", ran.out.into()),
            ]));
        }
        let mut hashes = Vec::new();
        let mut name: Option<String> = None;
        for line in ran.out.lines() {
            if let Some(rest) = line.strip_prefix("Entering '") {
                name = Some(rest.strip_suffix('\'').unwrap_or(rest).to_string());
            } else if line.trim().len() == 40 {
                let Some(n) = name.take() else {
                    return Err(Failure::new());
                };
                hashes.push((n, line.trim().to_string()));
            } else {
                let msg = format!("Unable to parse submodule hash line: {}", line.trim());
                return Err(failure(&[("msg", msg.into())]));
            }
        }
        if let Some(n) = name {
            let msg = format!("Unable to find hash for submodule: {n}");
            return Err(failure(&[("msg", msg.into())]));
        }
        hashes.sort();
        Ok(hashes)
    }

    /// `submodules_fetch`: whether a submodule is new or behind.
    pub(super) fn submodules_fetch(&self, dest: &Path, track: bool) -> Result<bool, Failure> {
        let Ok(text) = std::fs::read_to_string(dest.join(".gitmodules")) else {
            return Ok(false);
        };
        let added = text.lines().any(|line| {
            line.trim().starts_with("path")
                && line
                    .split_once('=')
                    .is_some_and(|(_, path)| !dest.join(path.trim()).join(".git").exists())
        });
        if added {
            return Ok(true);
        }
        let begin = self.submodule_hashes(dest, "HEAD")?;
        let git = self.path.display().to_string();
        self.check(&["submodule", "foreach", &git, "fetch"], Some(dest))?;
        if track {
            let after = self.submodule_hashes(dest, &format!("{REMOTE}/master"))?;
            return Ok(begin != after);
        }
        let status = self.check(&["submodule", "status"], Some(dest))?;
        Ok(status.out.lines().any(|l| !l.starts_with(' ')))
    }

    pub(super) fn submodule_update(
        &self,
        dest: &Path,
        track: bool,
        force: bool,
    ) -> Result<(), Failure> {
        self.check(&["submodule", "sync"], Some(dest))?;
        let mut args = vec!["submodule", "update", "--init", "--recursive"];
        if track {
            args.push("--remote");
        }
        if force {
            args.push("--force");
        }
        let ran = self.run(&args, Some(dest));
        if ran.rc != 0 {
            let msg = format!("Failed to init/update submodules: {}{}", ran.out, ran.err);
            return Err(failure(&[("msg", msg.into())]));
        }
        Ok(())
    }
}
