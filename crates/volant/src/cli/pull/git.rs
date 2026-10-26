// SPDX-License-Identifier: GPL-3.0-or-later
//! The checkout step of `pull`: what the `git` module of ansible-core 2.19.12 (`modules/git.py`)
//! does with the arguments `ansible-pull` gives it, run with the system `git` and no Python.
//! Only those arguments exist here: remote `origin`, no refspec, not bare, submodules followed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

/// What `ansible-pull` hands the module.
pub(super) struct Repo<'a> {
    pub url: &'a str,
    pub dest: &'a Path,
    /// `version`: `HEAD` unless `-C` names a branch, a tag or a commit.
    pub version: &'a str,
    /// `depth=1`, unless `--full`.
    pub shallow: bool,
    /// `force=yes`: `--clean`.
    pub force: bool,
    /// `verify_commit=yes`: `--verify-commit`.
    pub verify: bool,
    /// `track_submodules=yes`: `--track-subs`.
    pub track_subs: bool,
    /// `accept_hostkey=yes`: `--accept-host-key`.
    pub accept_host_key: bool,
    /// `key_file=`: `--private-key`.
    pub key_file: Option<&'a Path>,
}

/// The module's answer: its result, whether it failed, and the warnings it raised.
pub(super) struct Answer {
    pub result: Map<String, Value>,
    pub failed: bool,
    pub warnings: Vec<String>,
}

/// A `fail_json`: the fields of the failed result.
type Failure = Map<String, Value>;

mod finish;
mod switch;

const REMOTE: &str = "origin";

/// Runs the module.
pub(super) fn checkout(repo: &Repo<'_>) -> Answer {
    let mut warnings = Vec::new();
    let (failed, mut result) = match run(repo, &mut warnings) {
        Ok(result) => (false, result),
        Err(failure) => (true, failure),
    };
    result.entry("changed").or_insert(Value::Bool(false));
    for (text, lines) in [("stdout", "stdout_lines"), ("stderr", "stderr_lines")] {
        if let Some(Value::String(s)) = result.get(text) {
            let split: Vec<Value> = s.lines().map(Value::from).collect();
            result.insert(lines.into(), split.into());
        }
    }
    Answer {
        result,
        failed,
        warnings,
    }
}

fn run(repo: &Repo<'_>, warnings: &mut Vec<String>) -> Result<Map<String, Value>, Failure> {
    let git = Git::new(repo)?;
    let url = file_url(repo.url);
    let dest = repo.dest;
    let version = repo.version;
    let mut result = Map::new();
    result.insert("changed".into(), false.into());
    result.insert("before".into(), Value::Null);
    let mut local_mods = false;
    let mut remote_url_changed = false;
    if dest.join(".git").join("config").exists() {
        local_mods = git.has_local_mods(dest);
        let before = git.version_of(dest);
        result.insert("before".into(), before.into());
        if local_mods {
            let msg = format!(
                "Local modifications exist in the destination: {}",
                dest.display()
            );
            if !repo.force {
                let mut failure = result;
                failure.insert("msg".into(), format!("{msg} (force=no).").into());
                return Err(failure);
            }
            git.check(&["reset", "--hard", "HEAD"], Some(dest))?;
            result.insert("changed".into(), true.into());
            result.insert("msg".into(), msg.into());
        }
        remote_url_changed = git.set_remote_url(&url, dest)?;
        result.insert("remote_url_changed".into(), remote_url_changed.into());
        git.fetch(&url, dest, version, repo.shallow, repo.force)?;
    } else {
        git.clone(&url, dest, version, repo.shallow, warnings)?;
        if repo.verify {
            git.verify(dest, version)?;
        }
    }
    git.switch_version(dest, version, repo.shallow)?;
    if repo.verify {
        git.verify(dest, version)?;
    }
    let submodules = git.submodules_fetch(dest, repo.track_subs)?;
    if submodules {
        result.insert("submodules_changed".into(), true.into());
        git.submodule_update(dest, repo.track_subs, repo.force)?;
    }
    let after = git.version_of(dest);
    let before = result.get("before").and_then(Value::as_str);
    if before != Some(after.as_str()) || local_mods || submodules || remote_url_changed {
        result.insert("changed".into(), true.into());
    }
    result.insert("after".into(), after.into());
    Ok(result)
}

/// A path, as the module makes it a URL so that `depth` works: `file://` and the expanded path.
fn file_url(url: &str) -> String {
    let expanded = match url.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            std::env::var("HOME").map_or_else(|_| url.to_string(), |home| format!("{home}{rest}"))
        }
        _ => url.to_string(),
    };
    if expanded.starts_with('/') {
        format!("file://{expanded}")
    } else {
        url.to_string()
    }
}

fn failure(pairs: &[(&str, Value)]) -> Failure {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// What a finished command printed and returned.
struct Ran {
    rc: i32,
    out: String,
    err: String,
}

struct Git {
    path: PathBuf,
    /// `GIT_SSH_COMMAND`, as `set_git_ssh_env` builds it.
    ssh: String,
}

impl Git {
    /// `get_bin_path('git')`, which also looks in the `sbin` directories, and the ssh command.
    fn new(repo: &Repo<'_>) -> Result<Self, Failure> {
        let mut dirs: Vec<String> = Vec::new();
        let path_var = std::env::var("PATH").unwrap_or_default();
        for dir in path_var
            .split(':')
            .map(str::to_string)
            .chain(["/sbin", "/usr/sbin", "/usr/local/sbin"].map(String::from))
        {
            if !dirs.contains(&dir) && Path::new(&dir).exists() {
                dirs.push(dir);
            }
        }
        let found = dirs
            .iter()
            .map(|d| Path::new(d).join("git"))
            .find(|p| p.is_file() && executable(p));
        let Some(path) = found else {
            return Err(failure(&[(
                "msg",
                format!(
                    "Failed to find required executable \"git\" in paths: {}",
                    dirs.join(":")
                )
                .into(),
            )]));
        };
        let mut opts = std::env::var("GIT_SSH_OPTS").unwrap_or_default();
        if repo.accept_host_key {
            opts.push_str(" -o StrictHostKeyChecking=no");
        }
        opts.push_str(" -o BatchMode=yes");
        if let Some(key) = repo.key_file {
            let _ = write!(opts, "  -i {} -o IdentitiesOnly=yes", key.display());
        }
        let ssh = std::env::var("GIT_SSH")
            .or_else(|_| std::env::var("GIT_SSH_COMMAND"))
            .unwrap_or_else(|_| "ssh".into());
        Ok(Self {
            path,
            ssh: format!("{ssh} {opts}"),
        })
    }

    /// `run_command`: the C locale, and `cwd` only when it is a directory.
    fn run(&self, args: &[&str], cwd: Option<&Path>) -> Ran {
        let mut command = Command::new(&self.path);
        command
            .args(args)
            .stdin(Stdio::null())
            .env("GIT_SSH_COMMAND", &self.ssh);
        for name in ["LANG", "LC_ALL", "LC_MESSAGES", "LC_CTYPE", "LANGUAGE"] {
            command.env(name, "C");
        }
        if let Some(dir) = cwd.filter(|d| d.is_dir()) {
            command.current_dir(dir);
        }
        match command.output() {
            Ok(done) => Ran {
                rc: done.status.code().unwrap_or(-1),
                out: String::from_utf8_lossy(&done.stdout).into_owned(),
                err: String::from_utf8_lossy(&done.stderr).into_owned(),
            },
            Err(e) => Ran {
                rc: e.raw_os_error().unwrap_or(2),
                out: String::new(),
                err: e.to_string(),
            },
        }
    }

    /// `run_command(check_rc=True)`: a command that fails fails the module with its output.
    fn check(&self, args: &[&str], cwd: Option<&Path>) -> Result<Ran, Failure> {
        let ran = self.run(args, cwd);
        if ran.rc == 0 {
            return Ok(ran);
        }
        let cmd = std::iter::once(self.path.display().to_string())
            .chain(args.iter().map(|a| (*a).to_string()))
            .map(|w| crate::transport::trace::shell_quote(&w))
            .collect::<Vec<_>>()
            .join(" ");
        Err(failure(&[
            ("cmd", cmd.into()),
            ("msg", ran.err.trim_end().into()),
            ("rc", ran.rc.into()),
            ("stderr", ran.err.into()),
            ("stdout", ran.out.into()),
        ]))
    }

    /// The command as a string, the way the module writes the ones it formats itself.
    fn line(&self, args: &[&str]) -> String {
        format!("{} {}", self.path.display(), args.join(" "))
    }

    /// `get_version`: `git rev-parse HEAD`, whatever it returns.
    fn version_of(&self, dest: &Path) -> String {
        let ran = self.run(&["rev-parse", "HEAD"], Some(dest));
        ran.out.trim_end_matches('\n').to_string()
    }

    fn has_local_mods(&self, dest: &Path) -> bool {
        let ran = self.run(&["status", "--porcelain"], Some(dest));
        ran.out.lines().any(|line| !line.starts_with("??"))
    }

    fn is_remote_branch(&self, cwd: &Path, remote: &str, version: &str) -> Result<bool, Failure> {
        let head = format!("refs/heads/{version}");
        let ran = self.check(&["ls-remote", remote, "-h", &head], Some(cwd))?;
        Ok(ran.out.contains(version))
    }

    fn is_remote_tag(&self, cwd: &Path, remote: &str, version: &str) -> Result<bool, Failure> {
        let tag = format!("refs/tags/{version}");
        let ran = self.check(&["ls-remote", remote, "-t", &tag], Some(cwd))?;
        Ok(ran.out.contains(version))
    }

    fn clone(
        &self,
        url: &str,
        dest: &Path,
        version: &str,
        shallow: bool,
        warnings: &mut Vec<String>,
    ) -> Result<(), Failure> {
        let parent = dest.parent().unwrap_or(Path::new("/"));
        let _ = std::fs::create_dir_all(parent);
        let branch_or_tag =
            self.is_remote_branch(dest, url, version)? || self.is_remote_tag(dest, url, version)?;
        let mut args = vec!["clone", "--origin", REMOTE];
        if shallow {
            if version == "HEAD" {
                args.extend(["--depth", "1"]);
            } else if branch_or_tag {
                args.extend(["--depth", "1", "--branch", version]);
            } else {
                warnings.push(
                    "Ignoring depth argument. Shallow clones are only available for HEAD, \
                     branches, tags or in combination with refspec."
                        .into(),
                );
            }
        }
        let dest_text = dest.display().to_string();
        args.extend([url, dest_text.as_str()]);
        self.check(&args, Some(parent))?;
        Ok(())
    }

    fn remote_url(&self, dest: &Path) -> Option<String> {
        let ran = self.run(&["ls-remote", "--get-url", REMOTE], Some(dest));
        (ran.rc == 0).then(|| ran.out.trim_end_matches('\n').to_string())
    }

    /// `set_remote_url`: whether the remote's URL had to change.
    fn set_remote_url(&self, url: &str, dest: &Path) -> Result<bool, Failure> {
        let current = self.remote_url(dest);
        if current.as_deref().is_some_and(|c| same_url(c, url, dest)) {
            return Ok(false);
        }
        let ran = self.run(&["remote", "set-url", REMOTE, url], Some(dest));
        if ran.rc != 0 {
            let msg = format!(
                "Failed to set a new url {url} for {REMOTE}: {} {}",
                ran.out, ran.err
            );
            return Err(failure(&[("msg", msg.into())]));
        }
        Ok(current.is_some())
    }

    fn fetch(
        &self,
        url: &str,
        dest: &Path,
        version: &str,
        shallow: bool,
        force: bool,
    ) -> Result<(), Failure> {
        self.set_remote_url(url, dest)?;
        let mut args: Vec<String> = vec!["fetch".into()];
        let mut refspecs: Vec<String> = Vec::new();
        if shallow {
            let current = self.head_branch(dest)?;
            if version == "HEAD" {
                refspecs.push(current.unwrap_or_else(|| "None".into()));
            } else if self.is_remote_branch(dest, url, version)? {
                if current.as_deref() != Some(version) {
                    refspecs.push(format!("+refs/heads/{version}:refs/heads/{version}"));
                }
                refspecs.push(format!(
                    "+refs/heads/{version}:refs/remotes/{REMOTE}/{version}"
                ));
            } else if self.is_remote_tag(dest, url, version)? {
                refspecs.push(format!("+refs/tags/{version}:refs/tags/{version}"));
            }
            if !refspecs.is_empty() {
                args.extend(["--depth".into(), "1".into()]);
            }
        }
        if !shallow || refspecs.is_empty() {
            args.push("--tags".into());
        }
        if force {
            args.push("--force".into());
        }
        args.push(REMOTE.into());
        args.extend(refspecs);
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        let ran = self.run(&words, Some(dest));
        if ran.rc != 0 {
            let cmd: Vec<Value> = std::iter::once(self.path.display().to_string())
                .chain(args)
                .map(Value::from)
                .collect();
            let msg = format!(
                "Failed to download remote objects and refs: {} {}",
                ran.out, ran.err
            );
            return Err(failure(&[("cmd", cmd.into()), ("msg", msg.into())]));
        }
        Ok(())
    }
}

/// `unfrackgitpath` of both sides, as the module compares a remote's URL with the one asked
/// for: equal text, or the same path once made absolute from `dest`.
fn same_url(current: &str, wanted: &str, dest: &Path) -> bool {
    let unfrack = |u: &str| {
        let path = dest.join(u);
        std::fs::canonicalize(&path).unwrap_or(path)
    };
    current == wanted || unfrack(current) == unfrack(wanted)
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(_: &Path) -> bool {
    true
}
