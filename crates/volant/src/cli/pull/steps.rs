// SPDX-License-Identifier: GPL-3.0-or-later
//! The two steps `ansible-pull` runs as children: the checkout, with the native `git` or the
//! repository module, and the playbook.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use super::PullArgs;
use super::git::{Repo, checkout};
use crate::cli::adhoc::Adhoc;
use crate::render::{ErrorBlock, ansible_json_indented, block_text};

/// The checkout with the native `git`, printed as `ansible -m git` prints it under `ansible-pull`:
/// plain, warnings and the error block first, all on standard output. Returns the exit code and
/// what went to the step's standard output, which `-o` reads.
pub(super) fn native_checkout(
    args: &PullArgs,
    url: &str,
    dest: &Path,
    limit: &str,
) -> (i32, String) {
    let mut out = String::new();
    if args.verbose >= 1 {
        out.push_str(&crate::cli::announce::config_line());
        out.push('\n');
        print!("{out}");
    }
    let localhost = ["localhost", "127.0.0.1", "::1"];
    for name in limit.split(',').filter(|n| !localhost.contains(n)) {
        println!("[WARNING]: Could not match supplied host pattern, ignoring: {name}");
    }
    let answer = checkout(&Repo {
        url,
        dest,
        version: args.checkout.as_deref().unwrap_or("HEAD"),
        shallow: !args.full,
        force: args.clean,
        verify: args.verify_commit,
        track_subs: args.track_subs,
        accept_host_key: args.accept_host_key,
        key_file: args.connect.private_key.as_deref(),
    });
    for warning in &answer.warnings {
        println!("[WARNING]: {warning}");
    }
    let state = match (answer.failed, answer.result.get("changed")) {
        (true, _) => "FAILED!",
        (false, Some(Value::Bool(true))) => "CHANGED",
        _ => "SUCCESS",
    };
    if answer.failed {
        let msg = answer
            .result
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("Unknown error.");
        let block = ErrorBlock {
            message: format!("Task failed: Module failed: {msg}"),
            origin: Some(repo_task(args, url, dest).task_origin()),
            unknown: None,
            causes: Vec::new(),
            lead: None,
            on_stderr: false,
        };
        let text = block_text(&block);
        println!("{text}{}", if text.contains('\n') { "\n" } else { "" });
    }
    let line = format!(
        "localhost | {state} => {}\n",
        ansible_json_indented(&Value::Object(answer.result), 0)
    );
    print!("{line}");
    out.push_str(&line);
    (if answer.failed { 2 } else { 0 }, out)
}

/// The task `ansible-pull` builds for the module: what the error block shows.
fn repo_task(args: &PullArgs, url: &str, dest: &Path) -> Adhoc {
    Adhoc::new("all", &args.module_name, &repo_options(args, url, dest))
        .expect("the options are key=value words")
}

/// The module's options as `ansible-pull` writes them into `-a`.
pub(super) fn repo_options(args: &PullArgs, url: &str, dest: &Path) -> String {
    let dest = dest.display();
    let mut opts = match args.module_name.as_str() {
        "git" | "bzr" => format!("name={url} dest={dest}"),
        _ => format!("repo={url} dest={dest}"),
    };
    let module = args.module_name.as_str();
    if let Some(rev) = &args.checkout {
        let key = if ["subversion", "hg"].contains(&module) {
            "revision"
        } else {
            "version"
        };
        let _ = write!(opts, " {key}={rev}");
    }
    if module == "git" {
        for (on, text) in [
            (args.accept_host_key, " accept_hostkey=yes".to_string()),
            (
                args.connect.private_key.is_some(),
                format!(
                    " key_file={}",
                    args.connect
                        .private_key
                        .as_deref()
                        .unwrap_or(Path::new(""))
                        .display()
                ),
            ),
            (args.verify_commit, " verify_commit=yes".into()),
            (args.track_subs, " track_submodules=yes".into()),
            (!args.full, " depth=1".into()),
        ] {
            if on {
                opts.push_str(&text);
            }
        }
    } else if module == "subversion" && !args.full {
        opts.push_str(" export=yes");
    }
    if args.clean {
        opts.push_str(" force=yes");
    }
    opts
}

/// The checkout of `subversion`, `hg` and `bzr`: `ansible -m <module>`, which runs the module
/// with Python, its standard output copied and kept for `-o`.
pub(super) fn module_checkout(
    args: &PullArgs,
    url: &str,
    dest: &Path,
    limit: &str,
) -> (i32, String) {
    let mut command = volant_as("ansible", "adhoc");
    command.args(["-i", "localhost,", "-c", "local"]);
    verbosity(&mut command, args.verbose);
    command
        .args([
            "-m",
            &args.module_name,
            "-a",
            &repo_options(args, url, dest),
            "all",
            "-l",
            limit,
        ])
        .env("ANSIBLE_CALLBACK_RESULT_FORMAT", "json")
        .env("ANSIBLE_LOAD_CALLBACK_PLUGINS", "0");
    for vars in &args.extra_vars {
        command.args(["-e", vars]);
    }
    command.stdout(Stdio::piped());
    errors_to_stdout(&mut command);
    let Ok(mut child) = command.spawn() else {
        return (1, String::new());
    };
    let mut kept = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let mut chunk = [0_u8; 4096];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&chunk[..n]);
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(&chunk[..n]);
            let _ = stdout.flush();
        }
    }
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
    (code, String::from_utf8_lossy(&kept).into_owned())
}

/// The playbook step: `ansible-playbook -c local` in `dest`, with the options `ansible-pull`
/// hands on, its standard error on standard output. Returns its exit code.
pub(super) fn run_playbook(
    args: &PullArgs,
    dest: &Path,
    playbooks: &[PathBuf],
    limit: &str,
) -> i32 {
    let mut command = volant_as("ansible-playbook", "playbook");
    command.args(["-c", "local"]);
    verbosity(&mut command, args.verbose);
    command.args(playbooks);
    let pairs: [(&str, Vec<OsString>); 4] = [
        (
            "--vault-password-file",
            args.vault
                .vault_password_file
                .iter()
                .map(Into::into)
                .collect(),
        ),
        (
            "--vault-id",
            args.vault.vault_id.iter().map(Into::into).collect(),
        ),
        (
            "--become-password-file",
            args.become_password_file.iter().map(Into::into).collect(),
        ),
        (
            "--connection-password-file",
            args.connect
                .connection_password_file
                .iter()
                .map(Into::into)
                .collect(),
        ),
    ];
    for (flag, values) in pairs {
        for value in values {
            let mut joined = OsString::from(format!("{flag}="));
            joined.push(value);
            command.arg(joined);
        }
    }
    for vars in &args.extra_vars {
        command.args(["-e", vars]);
    }
    if args.ask_become_pass {
        command.arg("--ask-become-pass");
    }
    if !args.skip_tags.is_empty() {
        command.args(["--skip-tags", &args.skip_tags.join(",")]);
    }
    if !args.tags.is_empty() {
        command.args(["-t", &args.tags.join(",")]);
    }
    command.args(["-l", args.hosts.limit.as_deref().unwrap_or(limit)]);
    for (on, flag) in [
        (args.check, "-C"),
        (args.diff, "-D"),
        (args.flush_cache, "--flush-cache"),
    ] {
        if on {
            command.arg(flag);
        }
    }
    for source in &args.hosts.inventory {
        if source.contains(',') || dest.join(source).exists() {
            command.args(["-i", source]);
        }
    }
    command.current_dir(dest);
    errors_to_stdout(&mut command);
    command
        .status()
        .ok()
        .and_then(|status| status.code())
        .unwrap_or(1)
}

/// This program, started as the ansible-core program `name`: the subcommand `sub` with its
/// usage errors in that program's name. Its output is plain, as a child on a pipe prints it.
fn volant_as(name: &str, sub: &str) -> Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("volant"));
    let mut command = Command::new(exe);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let _ = sub;
        command.arg0(name);
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        command.arg(sub);
    }
    if std::env::var_os("ANSIBLE_FORCE_COLOR").is_none() {
        command.env("ANSIBLE_NOCOLOR", "1");
    }
    command
}

fn verbosity(command: &mut Command, level: u8) {
    if level > 0 {
        command.arg(format!("-{}", "v".repeat(level.into())));
    }
}

/// The child's standard error on this process's standard output, as `run_cmd` copies both.
fn errors_to_stdout(command: &mut Command) {
    let _ = std::io::stdout().flush();
    #[cfg(unix)]
    {
        use std::os::fd::AsFd as _;
        if let Ok(fd) = std::io::stdout().as_fd().try_clone_to_owned() {
            command.stderr(Stdio::from(fd));
        }
    }
    #[cfg(not(unix))]
    let _ = command;
}
