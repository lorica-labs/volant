// SPDX-License-Identifier: GPL-3.0-or-later
//! The branch the checkout ends on: `switch_version` and the branch lookups it makes.

use std::path::Path;

use super::{Failure, Git, REMOTE, failure};

impl Git {
    pub(super) fn branches(&self, dest: &Path) -> Result<Vec<String>, Failure> {
        let ran = self.run(&["branch", "--no-color", "-a"], Some(dest));
        if ran.rc != 0 {
            return Err(failure(&[
                (
                    "msg",
                    format!("Could not determine branch data - received {}", ran.out).into(),
                ),
                ("stderr", ran.err.into()),
                ("stdout", ran.out.into()),
            ]));
        }
        Ok(ran
            .out
            .split('\n')
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// `get_head_branch`: the branch `.git/HEAD` names, or with a detached head the one
    /// `refs/remotes/origin/HEAD` names.
    pub(super) fn head_branch(&self, dest: &Path) -> Result<Option<String>, Failure> {
        let detached = self.branches(dest)?.iter().any(|b| {
            b.starts_with("* ")
                && (b.contains("no branch")
                    || b.contains("detached from")
                    || b.contains("detached at"))
        });
        let repo = dest.join(".git");
        let file = if detached {
            repo.join("refs").join("remotes").join(REMOTE).join("HEAD")
        } else {
            repo.join("HEAD")
        };
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(None);
        };
        let first = text.split_inclusive('\n').next().unwrap_or_default();
        if first.is_empty() {
            return Ok(None);
        }
        let first = first.replacen(&format!("refs/remotes/{REMOTE}"), "", 1);
        let last = first.split(' ').next_back().unwrap_or_default();
        let name = last.splitn(3, '/').last().unwrap_or_default();
        Ok(Some(name.trim_end_matches('\n').to_string()))
    }

    pub(super) fn is_local_branch(&self, dest: &Path, branch: &str) -> Result<bool, Failure> {
        let branches = self.branches(dest)?;
        let current = format!("* {branch}");
        Ok(branches.iter().any(|b| b == branch || *b == current))
    }

    pub(super) fn switch_version(
        &self,
        dest: &Path,
        version: &str,
        shallow: bool,
    ) -> Result<(), Failure> {
        let words: Vec<String>;
        let mut what = format!("Failed to checkout {version}");
        if version == "HEAD" {
            let branch = self.head_branch(dest)?.unwrap_or_else(|| "None".into());
            self.checkout_branch(dest, &branch)?;
            what = format!("Failed to checkout branch {branch}");
            words = vec![
                "reset".into(),
                "--hard".into(),
                format!("{REMOTE}/{branch}"),
                "--".into(),
            ];
        } else if self.is_remote_branch(dest, REMOTE, version)? {
            if shallow && !self.is_local_branch(dest, version)? {
                self.fetch_branch(dest, version)?;
            }
            if self.is_local_branch(dest, version)? {
                self.checkout_branch(dest, version)?;
                words = vec![
                    "reset".into(),
                    "--hard".into(),
                    format!("{REMOTE}/{version}"),
                ];
            } else {
                words = vec![
                    "checkout".into(),
                    "--track".into(),
                    "-b".into(),
                    version.into(),
                    format!("{REMOTE}/{version}"),
                ];
            }
        } else {
            words = vec!["checkout".into(), "--force".into(), version.into()];
        }
        let args: Vec<&str> = words.iter().map(String::as_str).collect();
        let ran = self.run(&args, Some(dest));
        if ran.rc != 0 {
            return Err(failure(&[
                ("cmd", self.line(&args).into()),
                ("msg", what.into()),
                ("rc", ran.rc.into()),
                ("stderr", ran.err.into()),
                ("stdout", ran.out.into()),
            ]));
        }
        Ok(())
    }

    pub(super) fn checkout_branch(&self, dest: &Path, branch: &str) -> Result<(), Failure> {
        let ran = self.run(&["checkout", "--force", branch], Some(dest));
        if ran.rc != 0 {
            return Err(failure(&[
                ("msg", format!("Failed to checkout branch {branch}").into()),
                ("rc", ran.rc.into()),
                ("stderr", ran.err.into()),
                ("stdout", ran.out.into()),
            ]));
        }
        Ok(())
    }

    /// `set_remote_branch`: a shallow clone has only its own branch, so another is fetched first.
    pub(super) fn fetch_branch(&self, dest: &Path, version: &str) -> Result<(), Failure> {
        let local = format!("+refs/heads/{version}:refs/heads/{version}");
        let remote = format!("+refs/heads/{version}:refs/remotes/{REMOTE}/{version}");
        let ran = self.run(&["fetch", "--depth=1", REMOTE, &local, &remote], Some(dest));
        if ran.rc != 0 {
            return Err(failure(&[
                (
                    "msg",
                    format!("Failed to fetch branch from remote: {version}").into(),
                ),
                ("rc", ran.rc.into()),
                ("stderr", ran.err.into()),
                ("stdout", ran.out.into()),
            ]));
        }
        Ok(())
    }
}
