// SPDX-License-Identifier: GPL-3.0-or-later
//! The `playbook` command: the same arguments as `ansible-playbook`, for the subset that exists.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anstream::ColorChoice;
use clap::Parser;
use tokio::sync::watch;

use crate::compile::{self, Compiled};
use crate::config::Config;
use crate::executor::{self, RunOptions, RunState};
use crate::inventory::{Host, Inventory};
use crate::render::Renderer;
use crate::roles::RoleSearch;
use crate::stats::{Refusal, Stats, error_code, exit_code};
use crate::template::Templar;
use crate::transport::{ConnectionDefaults, Transport};
use crate::vars::VarStore;
use crate::{agent, playbook, preflight};

#[derive(Parser, Debug)]
pub struct PlaybookArgs {
    /// Playbook files, run in order.
    #[arg(required = true, value_name = "PLAYBOOK")]
    pub playbooks: Vec<PathBuf>,
    /// Inventory file. Without it only the implicit localhost exists.
    #[arg(short = 'i', long = "inventory", value_name = "PATH")]
    pub inventory: Option<PathBuf>,
    /// Extra variables: `key=value` pairs, inline JSON or YAML, or `@file`. Repeatable.
    #[arg(short = 'e', long = "extra-vars", value_name = "VARS")]
    pub extra_vars: Vec<String>,
    /// Limit the play to this host pattern.
    #[arg(short = 'l', long = "limit", value_name = "SUBSET")]
    pub limit: Option<String>,
    /// Disable coloured output.
    #[arg(long)]
    pub no_color: bool,
    /// Show module results for successful tasks too. Repeatable.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Log in to remote hosts as this user.
    #[arg(short = 'u', long = "user", value_name = "REMOTE_USER")]
    pub user: Option<String>,
    /// Private key file for ssh authentication.
    #[arg(long = "private-key", value_name = "PRIVATE_KEY_FILE")]
    pub private_key: Option<PathBuf>,
    /// Seconds to wait for a connection.
    #[arg(short = 'T', long = "timeout", value_name = "TIMEOUT")]
    pub timeout: Option<u64>,
    /// Number of hosts to run at once.
    #[arg(short = 'f', long = "forks", value_name = "FORKS")]
    pub forks: Option<usize>,
    /// Run tasks with privilege escalation.
    #[arg(short = 'b', long = "become")]
    pub r#become: bool,
    /// Escalate to this user instead of root.
    #[arg(long = "become-user", value_name = "USER")]
    pub become_user: Option<String>,
    /// Escalation method. Only `sudo` is implemented.
    #[arg(long = "become-method", value_name = "METHOD")]
    pub become_method: Option<String>,
    /// Ask for the escalation password on the terminal.
    #[arg(short = 'K', long = "ask-become-pass")]
    pub ask_become_pass: bool,
}

/// Runs the playbooks and returns the process exit code.
pub fn run(args: PlaybookArgs) -> i32 {
    let choice = if args.no_color {
        ColorChoice::Never
    } else {
        ColorChoice::Auto
    };
    let mut out = Renderer::new(choice, args.verbose);
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("ERROR! {err}");
            return 250;
        }
    };
    match runtime.block_on(run_all(&args, &mut out)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("ERROR! {err:#}");
            error_code(&err)
        }
    }
}

async fn run_all(args: &PlaybookArgs, out: &mut Renderer) -> anyhow::Result<i32> {
    let config = Config::load()?;
    // Refused before anything is loaded, the way the reference refuses it, whether it comes
    // from the command line, the environment or `ansible.cfg`. Exit 2: `Cli::parse()` already
    // exits 2 for `-f abc` or `-f -1` (clap's own default for a bad argument), so a refused `0`
    // through this check stays on the same code rather than inventing a second one for the same
    // flag; a refusal never prints a `PLAY` header or a recap, so it cannot be mistaken for a
    // failed task, which is exit 2's other meaning.
    let forks = args.forks.unwrap_or(config.forks);
    if forks == 0 {
        eprintln!("ERROR! The number of processes (--forks) must be >= 1");
        return Ok(2);
    }
    let inventory_path = args.inventory.clone().or_else(|| config.inventory.clone());
    let inventory = match &inventory_path {
        Some(path) => Inventory::load(path)?,
        None => Inventory::empty(),
    };
    let playbooks = args
        .playbooks
        .iter()
        .map(|p| playbook::load(p))
        .collect::<anyhow::Result<Vec<_>>>()?;
    // Every playbook is loaded first, then every one of them is checked, so an operator gets
    // the refusal for the second playbook before the first one has touched a host. Nothing
    // below this line may assume a keyword was handled that the pre-flight did not let past.
    for pb in &playbooks {
        preflight::check(pb)?;
    }
    // Roles are read and spliced here, before the first `PLAY` banner: a role nobody can find
    // stops the run with nothing printed, which is where the reference stops it too. The whole
    // compilation is wrapped in the code the reference gives a playbook it cannot make sense of,
    // and the refusals that measured a code of their own carry it through untouched.
    let mut compiled: Vec<Vec<Compiled>> = Vec::new();
    for pb in &playbooks {
        let mut plays = Vec::new();
        for play in &pb.plays {
            let search = RoleSearch::new(&play.dir, &config);
            plays.push(compile::compile(play, &search).map_err(|e| Refusal::or(4, e))?);
        }
        compiled.push(plays);
    }
    // The second half of the pre-flight: what a role or an imported file brought in is refused
    // by its own name too, and before the first connection like everything else.
    for plays in &compiled {
        for play in plays {
            preflight::check_steps(play)?;
        }
    }
    let agents = agent::AgentSource::discover();
    // Refused by name before a single host is reached, wherever the method came from.
    // Escalating with `sudo` because `su` is not implemented would run the task under rules the
    // operator never wrote, so this is a startup refusal and not a warning.
    //
    // It speaks only for a run that escalates: an `ansible.cfg` or an `ANSIBLE_BECOME_METHOD`
    // naming another program is no reason to refuse a playbook that never becomes anyone. What
    // this pass cannot see - a task keyword, or a variable arriving through `group_vars`,
    // `host_vars`, `--extra-vars` or a `set_fact` - is refused per task, as a failure, when
    // that task resolves its escalation.
    let become_method = args
        .become_method
        .clone()
        .unwrap_or(config.become_method.clone());
    let all_hosts = inventory.resolve("all").hosts;
    let escalates = args.r#become
        || config.r#become
        || playbooks
            .iter()
            .flat_map(|pb| &pb.plays)
            .any(|play| play.r#become == Some(true))
        || all_hosts.iter().any(|host| {
            host.vars
                .get("ansible_become")
                .and_then(executor::as_bool_value)
                == Some(true)
        });
    // Exit 2, the reference's own code, measured: `--become-method doas` there loads no plugin
    // for it and fails the task, which is exit 2. This refuses before the play instead, so no
    // recap is printed, but the code an operator's script reads is the same one.
    if escalates {
        if become_method != playbook::BECOME_METHOD {
            return Err(Refusal::at(
                2,
                format!("become_method '{become_method}' is not supported yet"),
            ));
        }
        for host in &all_hosts {
            if let Some(method) = host
                .vars
                .get("ansible_become_method")
                .and_then(|v| v.as_str())
                && method != playbook::BECOME_METHOD
            {
                return Err(Refusal::at(
                    2,
                    format!(
                        "host '{}': ansible_become_method '{method}' is not supported yet",
                        host.name
                    ),
                ));
            }
        }
    }
    let defaults = ConnectionDefaults {
        remote_user: args.user.clone().or(config.remote_user),
        private_key: args.private_key.clone().or(config.private_key_file),
        host_key_checking: config.host_key_checking,
        remote_tmp: config.remote_tmp,
        connect_timeout: args
            .timeout
            .map(std::time::Duration::from_secs)
            .unwrap_or(config.timeout),
        r#become: args.r#become || config.r#become,
        become_user: args
            .become_user
            .clone()
            .unwrap_or(config.become_user.clone()),
        become_method,
        become_password: if args.ask_become_pass {
            Some(ask_become_password()?)
        } else {
            None
        },
    };
    let mut stats = Stats::default();

    let (stop_tx, stop_rx) = watch::channel(false);
    spawn_signal_watcher(stop_tx);
    let options = RunOptions {
        defaults,
        forks,
        stop: stop_rx.clone(),
    };

    let playbook_dir = playbook::base_dir(&args.playbooks[0]);
    let cwd = std::env::current_dir()?;
    let extra = crate::vars::parse_extra_vars(&args.extra_vars, &cwd)?;
    let mut store = VarStore::new(&inventory, inventory_path.as_deref(), &playbook_dir, extra)?;
    store.set_forks(forks);
    let mut state = RunState {
        templar: Arc::new(Templar::new(playbook_dir.clone())),
        vars: Arc::new(Mutex::new(store)),
        failed_hosts: HashSet::new(),
        verbosity: args.verbose,
        links: HashMap::new(),
    };

    let limit: Option<HashSet<String>> = match &args.limit {
        None => None,
        Some(pattern) => {
            let resolution = inventory.resolve(pattern);
            // A `--limit` naming a host that is not there is a warning and not a refusal, the
            // way it is in a play's own `hosts`: measured, `-l web1,web-typo` warns
            // `Could not match supplied host pattern, ignoring: web-typo` and then runs on
            // `web1`. Losing it meant a mistyped name silently narrowed the run to nothing it
            // was asked about.
            for unmatched in &resolution.unmatched {
                out.warning(&format!(
                    "Could not match supplied host pattern, ignoring: {unmatched}"
                ));
            }
            let names: HashSet<String> = resolution.hosts.into_iter().map(|h| h.name).collect();
            if names.is_empty() {
                // Exit 1, which is the reference's own code here, measured.
                anyhow::bail!(
                    "Specified inventory, host pattern and/or --limit leaves us with no hosts to target."
                );
            }
            Some(names)
        }
    };

    // `resolve` recomputes inventory-load-time warnings (e.g. a host/group homonym) on every
    // call, matching Ansible's own semantics for a single resolution; but a playbook with N plays
    // calls `resolve` N times, and Ansible only ever prints such a warning once per run, at
    // inventory load. Deduplicating here, at the point of printing, keeps `resolve`'s contract
    // (its `Resolution.warnings` always reflects the truth for that one call, which callers other
    // than this CLI loop may rely on) while matching the reference's once-per-run output.
    let mut warned: HashSet<String> = HashSet::new();
    // A controller with no local agent can still drive remote hosts, so the local agent only
    // has to be there once a play actually targets a host that runs one here. Saying so before
    // that play starts beats one `UNREACHABLE` per local host.
    let mut local_agent_checked = false;
    let mut current_dir = playbook_dir;
    // Every way out of the loop below runs `shutdown_links` once, which is why the loop is a
    // block whose result is read afterwards rather than a stretch of `?`. `AgentLink::drop`
    // busy-polls for its agent for up to 200 ms, on the runtime thread and one link at a time,
    // so a `?` returning past the shutdown would pay that for every live link in series.
    let plays: anyhow::Result<()> = async {
        'plays: for (pb, steps) in playbooks.iter().zip(&compiled) {
            for (play, compiled) in pb.plays.iter().zip(steps) {
                if *stop_rx.borrow() {
                    break 'plays;
                }
                // Per play rather than per playbook argument: `import_playbook` splices another
                // file's plays in place, and each of them reads its `group_vars/`, its
                // `vars_files` and its lookups beside the file it was written in.
                if play.dir != current_dir {
                    state.templar = Arc::new(Templar::new(play.dir.clone()));
                    state.vars.lock().expect("vars lock").rebase(&play.dir)?;
                    current_dir = play.dir.clone();
                }
                let resolution = inventory.resolve(&play.hosts);
                for warning in &resolution.warnings {
                    if warned.insert(warning.clone()) {
                        out.warning(warning);
                    }
                }
                for pattern in &resolution.unmatched {
                    out.warning(&format!(
                        "Could not match supplied host pattern, ignoring: {pattern}"
                    ));
                }
                let hosts: Vec<Host> = match &limit {
                    Some(allowed) => resolution
                        .hosts
                        .into_iter()
                        .filter(|h| allowed.contains(&h.name))
                        .collect(),
                    None => resolution.hosts,
                };
                if !local_agent_checked
                    && hosts.iter().any(|h| {
                        matches!(
                            Transport::for_host(h, &options.defaults),
                            Ok(Transport::Local)
                        )
                    })
                {
                    agents.local()?;
                    local_agent_checked = true;
                }
                executor::run_play(
                    play, compiled, hosts, &agents, &options, &mut state, out, &mut stats,
                )
                .await?;
            }
            // One recap per playbook argument, as the reference prints it, and the counters carry
            // over: the second playbook's recap shows the whole run so far. An interrupted run
            // skips this one and prints its own below, so a stop never doubles the last recap.
            if *stop_rx.borrow() {
                break 'plays;
            }
            out.recap(&stats);
        }
        Ok(())
    }
    .await;
    // Closing them here rather than letting the state drop keeps the wait for each agent off a
    // blocking drop inside the runtime.
    state.shutdown_links().await;
    plays?;
    if *stop_rx.borrow() {
        eprintln!("[ERROR]: User interrupted execution");
        out.recap(&stats);
        return Ok(99);
    }
    // `exit_code` reads the whole run, whatever the recaps showed along the way.
    Ok(exit_code(&stats))
}

/// Reads the escalation password from the terminal, with the prompt on stderr so a redirected
/// stdout still carries only the run's own output.
///
/// The echo is turned off by the shell that reads the line rather than from here, and that
/// shell arms its `trap` before touching the terminal: a Ctrl-C, a Ctrl-\ or a `SIGTERM` while
/// the operator is typing reaches the whole foreground process group, so the shell restores the
/// echo on its way out even though this process is dying too. `QUIT` is in the list because
/// Ctrl-\ is a key an operator reaches for at a prompt that seems stuck, and it would otherwise
/// kill the shell with the echo still off, leaving a terminal that types nothing back. Doing
/// this from Rust would need a `Drop` that a signal never runs, and a terminal left with the
/// echo off is a poor parting gift. Where stdin is not a terminal, `stty` fails, its complaint
/// is dropped and the line is read as it comes.
#[cfg(unix)]
fn ask_become_password() -> anyhow::Result<String> {
    use std::process::Stdio;
    const READ_LINE: &str = "trap 'stty echo 2>/dev/null' EXIT INT QUIT TERM HUP\n\
         stty -echo 2>/dev/null\n\
         IFS= read -r password || exit 1\n\
         printf %s \"$password\"\n";
    eprint!("BECOME password: ");
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(READ_LINE)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| anyhow::anyhow!("reading the become password: {e}"))?;
    eprintln!();
    if !out.status.success() {
        anyhow::bail!("no become password was given");
    }
    // `from_utf8` rather than `from_utf8_lossy`: a password quietly rewritten with replacement
    // characters would be sent to `sudo` and refused, and the run would blame the operator.
    String::from_utf8(out.stdout).map_err(|_| anyhow::anyhow!("the become password is not UTF-8"))
}

#[cfg(not(unix))]
fn ask_become_password() -> anyhow::Result<String> {
    use std::io::BufRead;
    eprint!("BECOME password: ");
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        anyhow::bail!("no become password was given");
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Ctrl-C and SIGTERM both request a clean stop: running batches are cancelled, the recap is
/// printed, and the process exits with ansible-playbook's code 99.
fn spawn_signal_watcher(stop: watch::Sender<bool>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(term) => term,
                Err(_) => return,
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = stop.send(true);
    });
}
