// SPDX-License-Identifier: GPL-3.0-or-later
//! The `pull` command: `ansible-pull` of ansible-core 2.19.12 (`cli/pull.py`). The repository is
//! checked out with the system `git` and no Python, then the playbook it holds runs as
//! `ansible-playbook -c local` from the checkout. As in the reference, what both steps print goes
//! to standard output, and what the command itself warns about to standard error.

use std::hash::{BuildHasher as _, Hasher as _};
use std::path::{Path, PathBuf};

mod args;
mod git;
mod names;
mod steps;

pub use args::PullArgs;
use names::{first_label, host_names};
use steps::{module_checkout, native_checkout, run_playbook};

const MODULES: [&str; 4] = ["git", "subversion", "hg", "bzr"];

/// Runs `args` and returns the exit code.
pub fn pull(args: PullArgs) -> i32 {
    if args.version {
        println!("{}", super::version::block("ansible-pull", &[]));
        return 0;
    }
    match run(&args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("[ERROR]: {message}");
            5
        }
    }
}

/// The run; `Err` is an options error of the reference, exit 5.
fn run(args: &PullArgs) -> Result<i32, String> {
    let names = host_names();
    let dest = match &args.dest {
        Some(dest) => absolute(dest),
        None => ansible_home().join("pull").join(&names.fqdn),
    };
    if dest.exists() && !dest.is_dir() {
        return Err(format!(
            "{} is not a valid or accessible directory.",
            dest.display()
        ));
    }
    let sleep = match args.sleep.as_deref().filter(|s| !s.is_empty()) {
        None => 0,
        Some(text) => match text.trim().parse::<u64>() {
            Ok(bound) if bound > 0 => random_below(bound),
            _ => return Err(format!("{text} is not a number.")),
        },
    };
    let Some(url) = args.url.as_deref() else {
        return Err("URL for repository not specified, use -h for help".into());
    };
    if !MODULES.contains(&args.module_name.as_str()) {
        return Err(format!(
            "Unsupported repo module {}, choices are {}",
            args.module_name,
            MODULES.join(",")
        ));
    }
    if args.verbose >= 2 {
        println!("{}", super::version::block("ansible-pull", &[]));
    }
    if args.verbose >= 1 {
        println!("{}", super::announce::config_line());
    }
    println!(
        "{}",
        crate::template::local_now("Starting Ansible Pull at %F %T")
    );
    println!("{}", command_line());
    let limit = format!("localhost,{},127.0.0.1", names.all.join(","));
    if sleep > 0 {
        println!("Sleeping for {sleep} seconds...");
        std::thread::sleep(std::time::Duration::from_secs(sleep));
    }
    let (rc, out) = if args.module_name == "git" {
        native_checkout(args, url, &dest, &limit)
    } else {
        module_checkout(args, url, &dest, &limit)
    };
    if rc != 0 {
        if !args.force {
            return Ok(rc);
        }
        eprintln!(
            "[WARNING]: Unable to update repository. Continuing with (forced) run of playbook."
        );
    } else if args.only_if_changed
        && !["\"changed\": true", "changed: True", "| CHANGED =>"]
            .iter()
            .any(|sign| out.contains(sign))
    {
        println!(
            "Repository has not changed, quitting: {}",
            crate::transport::trace::bytes_repr(out.as_bytes())
        );
        return Ok(0);
    }
    let playbooks = select_playbooks(args, &dest, &names.fqdn)
        .ok_or_else(|| "Could not find a playbook to run.".to_string())?;
    let code = run_playbook(args, &dest, &playbooks, &limit);
    if args.purge
        && let Err(e) = std::fs::remove_dir_all(&dest)
    {
        eprintln!("[ERROR]: Failed to remove {}: {e}", dest.display());
    }
    Ok(code)
}

/// `sys.argv` joined by spaces: the program as the shell found it, then the arguments.
fn command_line() -> String {
    let mut argv = std::env::args_os();
    let program = argv.next().unwrap_or_default();
    let shown = if Path::new(&program).components().count() > 1 {
        PathBuf::from(&program)
    } else {
        std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(&program))
                    .find(|candidate| candidate.is_file())
            })
            .unwrap_or_else(|| PathBuf::from(&program))
    };
    std::iter::once(shown.into_os_string())
        .chain(argv)
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `secrets.randbelow(bound)`: the spread is all that matters, not the quality of the draw.
fn random_below(bound: u64) -> u64 {
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
        % bound
}

/// `ANSIBLE_HOME`, else `~/.ansible`.
fn ansible_home() -> PathBuf {
    std::env::var_os("ANSIBLE_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ansible")))
        .unwrap_or_else(|| PathBuf::from("~/.ansible"))
}

/// `unfrack_path`: `~` expanded, made absolute, links resolved where the path exists.
fn absolute(path: &Path) -> PathBuf {
    let expanded = match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    };
    let made = std::path::absolute(&expanded).unwrap_or(expanded);
    std::fs::canonicalize(&made).unwrap_or(made)
}

/// `select_playbook`: the playbooks named, each under `dest`, or the first of `<fqdn>.yml`,
/// `<hostname>.yml` and `local.yml` that exists. What is missing is one warning.
fn select_playbooks(args: &PullArgs, dest: &Path, fqdn: &str) -> Option<Vec<PathBuf>> {
    let tried = |path: &Path| -> Result<(), String> {
        let problem = if !path.exists() {
            "File does not exist"
        } else if std::fs::File::open(path).is_err() {
            "File is not readable"
        } else {
            return Ok(());
        };
        Err(format!("{}: {problem}", path.display()))
    };
    let mut errors = Vec::new();
    if args.playbooks.is_empty() {
        for name in [
            format!("{fqdn}.yml"),
            format!("{}.yml", first_label(fqdn)),
            "local.yml".into(),
        ] {
            let path = dest.join(name);
            match tried(&path) {
                Ok(()) => return Some(vec![path]),
                Err(e) => errors.push(e),
            }
        }
        eprintln!("[WARNING]: {}\n", errors.join("\n"));
        return None;
    }
    let mut found = Vec::new();
    for book in &args.playbooks {
        let path = dest.join(book);
        match tried(&path) {
            Ok(()) => found.push(path),
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        return Some(found);
    }
    let text = errors.join("\n");
    eprintln!(
        "[WARNING]: {text}{}",
        if errors.len() > 1 { "\n" } else { "" }
    );
    None
}

#[cfg(test)]
mod tests;
