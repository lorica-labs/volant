// SPDX-License-Identifier: GPL-3.0-or-later

//! The module's `systemctl` and what every result carries besides its own keys.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::Exit;
use crate::natives::common::{clean_args, popen_error};
use crate::natives::runtime::{self, Clock, Root, Stop};

/// The module's `systemctl`, and what bounds each run of it.
pub(super) struct Host<'a> {
    pub(super) systemctl: PathBuf,
    /// Set over the agent's environment for every command: the task's `environment`, and the
    /// `XDG_RUNTIME_DIR` the module adds when it has none.
    pub(super) env: BTreeMap<String, String>,
    /// The module's `PATH`, for `get_bin_path`.
    pub(super) path: BTreeMap<String, String>,
    pub(super) clock: Clock<'a>,
}

impl Host<'_> {
    /// `module.run_command(...)` on `systemctl args...`: exit code, output, error output.
    pub(super) fn systemctl(
        &self,
        reply: &Reply,
        args: &[&str],
    ) -> Result<(i32, String, String), Exit> {
        self.run(reply, &self.systemctl, args)
    }

    /// A program that cannot be started is handed back while nothing has changed; after that it
    /// fails the way `run_command` fails when `Popen` raises.
    pub(super) fn run(
        &self,
        reply: &Reply,
        program: &Path,
        args: &[&str],
    ) -> Result<(i32, String, String), Exit> {
        match runtime::run_output(&self.env, self.clock, program, args)? {
            Ok(ran) => Ok(ran),
            Err(_) if !reply.touched => {
                Err(format!("{} could not be started", program.display()).into())
            }
            Err(errno) => {
                let program = program.to_string_lossy();
                let words: Vec<&str> = std::iter::once(program.as_ref())
                    .chain(args.iter().copied())
                    .collect();
                let mut failure = reply.fail("Error executing command.".into());
                failure.insert("rc".into(), Value::from(errno));
                failure.insert("stdout".into(), Value::from(""));
                failure.insert("stderr".into(), Value::from(""));
                failure.insert("cmd".into(), Value::from(clean_args(&words)));
                failure.insert("exception".into(), popen_error(errno, &program));
                Err(Exit::Failed(failure))
            }
        }
    }

    /// `is_chroot(module)`: `/` against pid 1's root, or, when that cannot be read (not root),
    /// the inode of `/` against the file system's own root inode.
    pub(super) fn is_chroot(&self, reply: &Reply) -> Result<bool, Exit> {
        let root = std::fs::metadata("/").map_err(|err| format!("/ cannot be read: {err}"))?;
        if let Ok(init) = std::fs::metadata("/proc/1/root/.") {
            return Ok(root.ino() != init.ino() || root.dev() != init.dev());
        }
        let mut fs_root_ino = 2;
        if let Some(stat) = runtime::bin_path(&Root::real(), &self.path, "stat") {
            let (_, out, _) = self.run(reply, &stat, &["-f", "--format=%T", "/"])?;
            if out.contains("btrfs") {
                fs_root_ino = 256;
            } else if out.contains("xfs") {
                fs_root_ino = 128;
            }
        }
        Ok(root.ino() != fs_root_ino)
    }
}

/// What every result carries besides the module's own keys.
pub(super) struct Reply {
    pub(super) invocation: Value,
    pub(super) warnings: Vec<String>,
    /// Whether a command that changes the host has run; from then on nothing is handed back.
    pub(super) touched: bool,
}

impl Reply {
    pub(super) fn finish(&self, mut result: Map<String, Value>) -> Map<String, Value> {
        if !self.warnings.is_empty() {
            result.insert("warnings".into(), Value::from(self.warnings.clone()));
        }
        result.insert("invocation".into(), self.invocation.clone());
        result
    }

    /// `module.fail_json(msg=...)`.
    pub(super) fn fail(&self, msg: String) -> Map<String, Value> {
        let mut result = Map::new();
        result.insert("failed".into(), Value::Bool(true));
        result.insert("msg".into(), Value::String(msg));
        self.finish(result)
    }

    /// Hands the task back while nothing has changed; once something has, the native goes on
    /// the way the module does.
    pub(super) fn outside(&self, reason: &str) -> Result<(), Stop> {
        if self.touched {
            Ok(())
        } else {
            Err(reason.into())
        }
    }
}
