// SPDX-License-Identifier: GPL-3.0-or-later
//! The `playbook` command: the same arguments as `ansible-playbook`, for the subset that exists.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anstream::ColorChoice;
use clap::Parser;
use tokio::sync::watch;

use crate::bridge::Bridge;
use crate::compile::{self, Compiled, TagSelection};
use crate::config::Config;
use crate::executor::{self, Abort, RunOptions, RunState};
use crate::inventory::{Host, Inventory};
use crate::listing::{self, Listing};
use crate::profile::{Phase, Profile, micros};
use crate::python::{Facts, Natives};
use crate::render::Renderer;
use crate::roles::RoleSearch;
use crate::stats::{Refusal, Stats, error_code, exit_code};
use crate::template::Templar;
use crate::transport::{ConnectionDefaults, Transport};
use crate::vars::VarStore;
use crate::{agent, collection, facts_read, playbook, preflight, python};

#[derive(Parser, Debug)]
#[command(disable_version_flag = true)]
pub struct PlaybookArgs {
    /// Playbook files, run in order.
    #[arg(required_unless_present = "version", value_name = "PLAYBOOK")]
    pub playbooks: Vec<PathBuf>,
    /// Inventory file. Without it only the implicit localhost exists.
    #[arg(
        short = 'i',
        long = "inventory",
        alias = "inventory-file",
        value_name = "PATH"
    )]
    pub inventory: Option<PathBuf>,
    /// Extra variables: `key=value` pairs, inline JSON or YAML, or `@file`. Repeatable.
    #[arg(short = 'e', long = "extra-vars", value_name = "VARS")]
    pub extra_vars: Vec<String>,
    /// Limit the play to this host pattern.
    #[arg(short = 'l', long = "limit", value_name = "SUBSET")]
    pub limit: Option<String>,
    /// Only run tasks carrying these tags. Comma-separated, repeatable.
    #[arg(short = 't', long = "tags", value_name = "TAGS")]
    pub tags: Vec<String>,
    /// Only run tasks carrying none of these tags. Comma-separated, repeatable.
    #[arg(long = "skip-tags", value_name = "SKIP_TAGS")]
    pub skip_tags: Vec<String>,
    /// List the tasks the run would execute, and stop.
    #[arg(long = "list-tasks")]
    pub list_tasks: bool,
    /// List the tags the run would select, and stop.
    #[arg(long = "list-tags")]
    pub list_tags: bool,
    /// List the hosts each play would run on, and stop.
    #[arg(long = "list-hosts")]
    pub list_hosts: bool,
    /// Load and compile the playbooks without running anything.
    #[arg(long = "syntax-check")]
    pub syntax_check: bool,
    /// Disable coloured output.
    #[arg(long)]
    pub no_color: bool,
    /// Show module results for successful tasks too. Repeatable.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Directories searched for modules before the configured ones. Colon-separated, repeatable.
    #[arg(short = 'M', long = "module-path", value_name = "MODULE_PATH")]
    pub module_path: Vec<String>,
    /// Log in to remote hosts as this user.
    #[arg(short = 'u', long = "user", value_name = "REMOTE_USER")]
    pub user: Option<String>,
    /// Connection plugin to use.
    #[arg(short = 'c', long = "connection", value_name = "CONNECTION")]
    pub connection: Option<String>,
    /// Private key file for ssh authentication.
    #[arg(
        long = "private-key",
        alias = "key-file",
        value_name = "PRIVATE_KEY_FILE"
    )]
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
    /// Escalation method: `sudo` or `su`.
    #[arg(long = "become-method", value_name = "METHOD")]
    pub become_method: Option<String>,
    /// Ask for the escalation password on the terminal.
    #[arg(short = 'K', long = "ask-become-pass")]
    pub ask_become_pass: bool,
    /// Read the escalation password from the first line of this file.
    #[arg(
        long = "become-password-file",
        visible_alias = "become-pass-file",
        value_name = "BECOME_PASSWORD_FILE",
        conflicts_with = "ask_become_pass"
    )]
    pub become_password_file: Option<PathBuf>,
    /// Ask for the connection password on the terminal.
    #[arg(
        short = 'k',
        long = "ask-pass",
        conflicts_with = "connection_password_file"
    )]
    pub ask_pass: bool,
    /// Read the connection password from the first line of this file.
    #[arg(
        long = "connection-password-file",
        visible_alias = "conn-pass-file",
        value_name = "CONNECTION_PASSWORD_FILE"
    )]
    pub connection_password_file: Option<PathBuf>,
    /// Arguments added to every `ssh` command line.
    #[arg(long = "ssh-common-args", value_name = "SSH_COMMON_ARGS")]
    pub ssh_common_args: Option<String>,
    /// Arguments added to the `ssh` command lines only.
    #[arg(long = "ssh-extra-args", value_name = "SSH_EXTRA_ARGS")]
    pub ssh_extra_args: Option<String>,
    /// Accepted for `scp`, which Volant does not run: files move over the agent link.
    #[arg(long = "scp-extra-args", value_name = "SCP_EXTRA_ARGS")]
    pub scp_extra_args: Option<String>,
    /// Accepted for `sftp`, which Volant does not run: files move over the agent link.
    #[arg(long = "sftp-extra-args", value_name = "SFTP_EXTRA_ARGS")]
    pub sftp_extra_args: Option<String>,

    /// Ask for the vault password on the terminal.
    #[arg(
        short = 'J',
        long = "ask-vault-pass",
        visible_alias = "ask-vault-password"
    )]
    pub ask_vault_pass: bool,
    /// File, or executable printing the password, for the vault. Repeatable.
    #[arg(
        long = "vault-password-file",
        alias = "vault-pass-file",
        value_name = "VAULT_PASSWORD_FILE"
    )]
    pub vault_password_file: Vec<PathBuf>,
    /// Vault identity, `label@source` where the source is a file, an executable or `prompt`.
    /// Repeatable.
    #[arg(long = "vault-id", value_name = "VAULT_IDS")]
    pub vault_id: Vec<String>,

    /// Run handlers even if a task fails.
    #[arg(long = "force-handlers")]
    pub force_handlers: bool,

    /// Don't make any changes; instead, try to predict some of the changes that may occur.
    #[arg(short = 'C', long = "check")]
    pub check: bool,
    /// When changing (small) files and templates, show the differences in those files; works
    /// great with --check.
    #[arg(short = 'D', long = "diff")]
    pub diff: bool,

    /// Print where the run's time went on stderr, after the recap.
    #[arg(long = "profile")]
    pub profile: bool,
    /// Where `setup` gets its facts: `python` forces the Python module, `native` the agent's own
    /// collector, `auto` picks the native one only when the plays provably read nothing else.
    #[arg(long = "facts", value_enum, default_value_t = Facts::Auto)]
    pub facts: Facts,

    /// Start the run at the first task with this name; `*` and `?` match any text.
    #[arg(long = "start-at-task", value_name = "START_AT_TASK")]
    pub start_at_task: Option<String>,
    /// Ask before each task whether to run it. Needs a terminal.
    #[arg(long = "step")]
    pub step: bool,
    /// Accepted for compatibility: there is no fact cache plugin yet, so nothing is cleared.
    #[arg(long = "flush-cache")]
    pub flush_cache: bool,
    /// Print the version and the ansible-core release it matches, and stop.
    #[arg(long = "version")]
    pub version: bool,
}

/// What `--version` prints, and what `-vv` opens the run with in place of the reference's block
/// of machine paths: Volant's own version and the ansible-core release it matches.
fn version_line() -> String {
    format!(
        "volant {} (ansible-core {} compatible)",
        crate::VERSION,
        crate::vars::ansible_version()["full"]
            .as_str()
            .unwrap_or_default()
    )
}

/// The lines the reference prints before it reads an inventory: the version at `-vv`, the
/// configuration file at `-v`, and at `-vvvv` the plugin set-up.
fn announce(out: &mut Renderer) {
    if out.verbose_at(2) {
        out.plain(2, &version_line());
    }
    if out.verbose_at(1) {
        let absolute = |path: PathBuf| std::path::absolute(&path).unwrap_or(path);
        out.verbose(
            1,
            &match crate::config::locate() {
                Some(path) => format!("Using {} as config file", absolute(path).display()),
                None => "No config file found; using defaults".to_string(),
            },
        );
    }
    out.verbose(4, "setting up inventory plugins");
}

/// The plugins the reference tries on an inventory file before the `ini` one takes it, at
/// `-vvv`, and the two callbacks it sets aside at `-vv`. A `.yml` or `.json` file is the
/// `yaml` plugin's, which this release does not read.
fn announce_inventory(out: &mut Renderer, path: &Path) {
    // A `.yml` or `.json` file is the `yaml` plugin's, which this release does not read: no
    // line is better than a wrong plugin name.
    let yaml = path
        .extension()
        .is_some_and(|e| ["yaml", "yml", "json"].iter().any(|y| e == *y));
    if !path.is_file() || yaml || !out.verbose_at(3) {
        return;
    }
    let shown = std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string();
    let extension = path.extension().map(|e| e.to_string_lossy().into_owned());
    let others = extension
        .as_deref()
        .is_some_and(|e| !["yaml", "yml", "json"].contains(&e));
    let mut tried = vec!["host_list", "script"];
    if others {
        tried.extend(["auto", "yaml"]);
    } else {
        tried.push("auto");
    }
    for plugin in tried {
        out.verbose(
            3,
            &format!(
                "{plugin} declined parsing {shown} as it did not pass its verify_file() method"
            ),
        );
    }
    out.verbose(
        3,
        &format!("Parsed {shown} inventory source with ini plugin"),
    );
}

/// The options `-vvvv` lists under each playbook's banner. The reference lists every option it
/// holds a value for; this lists the ones every run has, so a run that sets others shows fewer.
fn options_dump(args: &PlaybookArgs, config: &Config, forks: usize) -> Vec<String> {
    let quoted = |items: Vec<String>| match items.as_slice() {
        [one] => format!("('{one}',)"),
        _ => format!(
            "({})",
            items
                .iter()
                .map(|i| format!("'{i}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let tags: Vec<String> = args
        .tags
        .iter()
        .flat_map(|t| crate::config::tag_list(t))
        .collect();
    let inventory = args
        .inventory
        .clone()
        .or_else(|| config.inventory.clone())
        .unwrap_or_else(|| PathBuf::from("/etc/ansible/hosts"));
    let inventory = std::path::absolute(&inventory).unwrap_or(inventory);
    let playbooks: Vec<String> = args
        .playbooks
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    vec![
        format!("Positional arguments: {}", playbooks.join(" ")),
        format!("verbosity: {}", args.verbose),
        format!(
            "connection: {}",
            args.connection.as_deref().unwrap_or("ssh")
        ),
        format!(
            "become_method: {}",
            args.become_method
                .as_deref()
                .unwrap_or(&config.become_method)
        ),
        format!(
            "tags: {}",
            quoted(if tags.is_empty() {
                vec!["all".to_string()]
            } else {
                tags
            })
        ),
        format!("inventory: ('{}',)", inventory.display()),
        format!("forks: {forks}"),
    ]
}

/// The `-M` directories as the reference reads them: each argument split on `:`, empty parts
/// dropped, every path made absolute. A later `-M` goes in front of the earlier ones (the
/// option prepends), and the order inside one argument stays.
fn module_path_dirs(args: &PlaybookArgs) -> Vec<PathBuf> {
    args.module_path
        .iter()
        .rev()
        .flat_map(|list| list.split(':'))
        .filter(|part| !part.is_empty())
        .map(|part| std::path::absolute(part).unwrap_or_else(|_| PathBuf::from(part)))
        .collect()
}

/// Runs the playbooks and returns the process exit code.
pub fn run(args: PlaybookArgs) -> i32 {
    if args.version {
        println!("{}", version_line());
        return 0;
    }
    // Before anything opens a descriptor: every host's link takes three of them.
    crate::transport::raise_open_file_limit();
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
    let profile = Arc::new(Profile::default());
    let code = match runtime.block_on(run_all(&args, &mut out, &profile)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("ERROR! {err:#}");
            error_code(&err)
        }
    };
    // A blocked terminal read (a `pause` prompt the run was interrupted at) must not hold the exit.
    runtime.shutdown_background();
    // After everything the run printed, recap included, and never on stdout.
    if args.profile {
        eprint!("{}", profile.render());
    }
    if let Some(path) = std::env::var_os("VOLANT_PROFILE_JSON").filter(|p| !p.is_empty())
        && let Err(err) = profile.write_json(Path::new(&path))
    {
        eprintln!(
            "[WARNING]: the profile could not be written to {}: {err}",
            Path::new(&path).display()
        );
    }
    code
}

async fn run_all(
    args: &PlaybookArgs,
    out: &mut Renderer,
    profile: &Arc<Profile>,
) -> anyhow::Result<i32> {
    let config = Config::load()?;
    announce(out);
    python::set_module_path(module_path_dirs(args));
    let secrets = match crate::vault::load(&crate::vault::Request {
        ask: args.ask_vault_pass,
        password_files: args.vault_password_file.clone(),
        ids: args.vault_id.clone(),
        config_file: config.vault_password_file.clone(),
        config_ids: config.vault_identity_list.clone(),
    }) {
        Ok(secrets) => secrets,
        Err(err) => {
            eprintln!("[WARNING]: {}", err.warning);
            eprintln!("[ERROR]: {}", err.message);
            return Ok(1);
        }
    };
    crate::vault::install(secrets);
    let show_custom_stats = config.show_custom_stats;
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
    let mut inventory = match &inventory_path {
        Some(path) => Inventory::load(path)?,
        None => Inventory::empty(),
    };
    if let Some(path) = &inventory_path {
        announce_inventory(out, path);
    }
    for callback in ["minimal", "oneline"] {
        out.verbose(
            2,
            &format!("Skipping callback '{callback}', as we already have a stdout callback."),
        );
    }
    let dump = if out.verbose_at(4) {
        options_dump(args, &config, forks)
    } else {
        Vec::new()
    };
    let compiling = std::time::Instant::now();
    let mut playbooks = args
        .playbooks
        .iter()
        .map(|p| playbook::load(p))
        .collect::<anyhow::Result<Vec<_>>>()?;
    // `--check` and `--diff` are each play's default: its own keyword wins, and every block,
    // role, task and handler under it inherits whichever applies, as in the reference.
    for play in playbooks.iter_mut().flat_map(|pb| pb.plays.iter_mut()) {
        play.check_mode = play
            .check_mode
            .take()
            .or(args.check.then_some(playbook::Flag::Fixed(true)));
        play.diff = play
            .diff
            .take()
            .or(args.diff.then_some(playbook::Flag::Fixed(true)));
    }
    let mode = Listing {
        tasks: args.list_tasks,
        tags: args.list_tags,
        hosts: args.list_hosts,
        syntax: args.syntax_check,
    };
    let selection = tag_selection(args, &config, mode);
    // Every playbook is loaded first, then every one of them is checked, so an operator gets
    // the refusal for the second playbook before the first one has touched a host. Nothing
    // below this line may assume a keyword was handled that the pre-flight did not let past.
    //
    // A listing skips this on purpose. It connects to nothing and runs nothing, so a playbook
    // this release cannot execute still lists, which is what lets an operator read a role
    // before the release that runs its modules. What it does not skip is the loader's own
    // refusals above, or the compilation's below: a playbook the reference cannot parse, or a
    // role nobody can find, is still refused here with the reference's own code.
    let checking = std::time::Instant::now();
    if !mode.wanted() {
        for pb in &playbooks {
            preflight::check(pb)?;
        }
    }
    let mut preflight_micros = micros(checking);
    // Roles are read and spliced here, before the first `PLAY` banner: a role nobody can find
    // stops the run with nothing printed, which is where the reference stops it too. The whole
    // compilation is wrapped in the code the reference gives a playbook it cannot make sense of,
    // and the refusals that measured a code of their own carry it through untouched.
    let mut compiled: Vec<Vec<Compiled>> = Vec::new();
    for pb in &playbooks {
        let mut plays = Vec::new();
        for play in &pb.plays {
            let search = RoleSearch::new(&play.dir, &config);
            plays.push(compile::compile(play, &search, &selection).map_err(|e| Refusal::or(4, e))?);
        }
        compiled.push(plays);
    }
    profile.phase(
        None,
        Phase::Compile,
        micros(compiling).saturating_sub(preflight_micros),
    );
    // The subset the whole run is narrowed to, resolved before anything else looks at a host.
    // The reference resolves it here too, before it reads a play: a `--limit` matching nothing
    // stops a listing as surely as it stops a run, measured, exit 1.
    let limit = resolve_limit(args, &inventory, out)?;
    // `resolve` recomputes inventory-load-time warnings (e.g. a host/group homonym) on every
    // call, matching Ansible's own semantics for a single resolution; but a playbook with N plays
    // calls `resolve` N times, and Ansible only ever prints such a warning once per run, at
    // inventory load. Deduplicating here, at the point of printing, keeps `resolve`'s contract
    // (its `Resolution.warnings` always reflects the truth for that one call, which callers other
    // than this CLI loop may rely on) while matching the reference's once-per-run output.
    let mut warned: HashSet<String> = HashSet::new();
    if mode.wanted() {
        let mut entries: Vec<listing::PlaybookEntry> = Vec::new();
        for (argument, (pb, steps)) in args.playbooks.iter().zip(playbooks.iter().zip(&compiled)) {
            let mut plays = Vec::new();
            for (play, compiled) in pb.plays.iter().zip(steps) {
                // Resolved even for `--list-tasks`, which never prints a host: the reference
                // warns about a pattern nothing matches whichever listing was asked for, and
                // that warning is the only sign an operator gets of a typo in `hosts:`.
                let hosts = play_hosts(&inventory, play, limit.as_ref(), out, &mut warned, false)?;
                plays.push(listing::PlayEntry {
                    play,
                    compiled,
                    hosts: hosts.into_iter().map(|h| h.name).collect(),
                });
            }
            entries.push(listing::PlaybookEntry {
                argument: argument.display().to_string(),
                plays,
            });
        }
        print!("{}", listing::render(&entries, mode));
        return Ok(0);
    }
    // `--start-at-task`: the first play that holds the task starts there, the plays in front of it
    // run nothing, and the plays behind it run whole. Static statements only, as the reference.
    let mut start_found = args.start_at_task.is_none();
    if let Some(pattern) = &args.start_at_task {
        for compiled in compiled.iter_mut().flatten() {
            if start_found {
                break;
            }
            compiled.start_at = Some(match compile::find_task(compiled, pattern) {
                Some(at) => {
                    start_found = true;
                    at
                }
                None => compiled.steps.len(),
            });
        }
    }
    // Asking needs somebody to answer. Refused before any host is touched, with the code the
    // reference's crash on a closed input has.
    if args.step && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!("[ERROR]: --step asks on the terminal, and standard input is not one");
        return Ok(250);
    }
    // The second half of the pre-flight: what a role or an imported file brought in is refused
    // by its own name too, and before the first connection like everything else.
    let checking = std::time::Instant::now();
    let terminal = std::io::IsTerminal::is_terminal(&std::io::stdin());
    for (pb, steps) in playbooks.iter().zip(&compiled) {
        for (play, compiled) in pb.plays.iter().zip(steps) {
            // Resolved quietly: `play_hosts` prints the pattern warnings when the play starts.
            let matched = inventory.resolve(&play.hosts).hosts.iter().any(|h| {
                limit
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(&h.name))
            });
            preflight::check_steps(compiled, terminal && matched)?;
        }
    }
    preflight_micros += micros(checking);
    profile.phase(None, Phase::Preflight, preflight_micros);
    let building = std::time::Instant::now();
    // Every Python module the compiled plays name or a dynamic include may read, built into one
    // payload before the first connection. Here rather than per play: one helper start, one
    // union, and a controller that cannot build payloads at all refuses the run now - with the
    // interpreters it tried - instead of connecting and failing the first Python task of the
    // first play.
    //
    // A run of native tasks alone names no Python module, builds nothing, and needs no
    // ansible-core on the controller.
    //
    // A collection's module the plays name is resolved by the same helper first, and refused
    // here, by name, when its collection is not installed or serves it through an action plugin.
    let named_dirs: Vec<PathBuf> = args
        .playbooks
        .iter()
        .map(PathBuf::as_path)
        .map(playbook::base_dir)
        .collect();
    let named_playbooks: Vec<python::Named> = named_dirs
        .iter()
        .zip(playbooks.iter().zip(&compiled))
        .map(|(dir, (pb, steps))| python::Named {
            dir: Some(dir.as_path()),
            plays: pb
                .plays
                .iter()
                .map(|play| play.dir.as_path())
                .zip(steps)
                .collect(),
        })
        .collect();
    let reach = python::Reach::walk_run(&named_playbooks).map_err(|e| Refusal::or(4, e))?;
    python::check_plugin_order(&reach).map_err(|e| Refusal::or(4, e))?;
    let python_modules = python::modules_for_run(&reach);
    let named: Vec<(String, String)> = compiled
        .iter()
        .flatten()
        .flat_map(preflight::collection_modules)
        .collect();
    let (python, helper) = python::union_and_helper(
        &python_modules,
        &named,
        &reach.plugin_dirs,
        &mut |warning| {
            out.warning(&warning, false);
        },
    )
    .map_err(|e| Refusal::or(4, e))?;
    for play in compiled.iter().flatten() {
        preflight::check_collection_actions(play).map_err(|e| Refusal::or(4, e))?;
    }
    // The filters, tests and lookups only ansible-core has run in the helper the union was
    // built with, or in one started on the first of them.
    let bridge = Bridge::new(reach.plugin_dirs.clone());
    out.set_diffs(Arc::clone(&bridge));
    if let Some(helper) = helper {
        bridge.adopt(helper);
    }
    // What `module_defaults` names, resolved by ansible-core as the reference resolves it when it
    // loads a play: a key nothing answers to, or a group nobody declares, stops the run before
    // anything is shown.
    let resolved = Arc::new(collection::Resolved::default());
    resolved
        .ensure(
            compiled
                .iter()
                .flatten()
                .flat_map(collection::tasks_of)
                .collect::<Vec<_>>(),
            Some(&bridge),
        )
        .map_err(|e| Refusal::at(4, e))?;
    for play in compiled.iter_mut().flatten() {
        play.resolved = Arc::clone(&resolved);
    }
    // Said once per run, as the reference says a warning once however many plays raise it.
    let mut said: HashSet<&String> = HashSet::new();
    for plays in &compiled {
        for play in plays {
            for warning in &play.warnings {
                if said.insert(warning) {
                    out.warning(warning, false);
                }
            }
            preflight::check_built(play, python.as_ref()).map_err(|e| Refusal::or(4, e))?;
        }
    }
    profile.phase(None, Phase::Union, micros(building));

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
            crate::vars::host_setting(&host.vars, "ansible_become")
                .and_then(executor::as_bool_value)
                == Some(true)
        });
    // Exit 2, the reference's own code, measured: `--become-method doas` there loads no plugin
    // for it and fails the task, which is exit 2. This refuses before the play instead, so no
    // recap is printed, but the code an operator's script reads is the same one.
    if escalates {
        if !playbook::BECOME_METHODS.contains(&become_method.as_str()) {
            return Err(Refusal::at(
                2,
                format!("become_method '{become_method}' is not supported yet"),
            ));
        }
        for host in &all_hosts {
            if let Some(method) = crate::vars::host_setting(&host.vars, "ansible_become_method")
                .and_then(|v| v.as_str())
                && !playbook::BECOME_METHODS.contains(&method)
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
    let (connection_password, become_password) = match ask_passwords(args) {
        Ok(passwords) => passwords,
        Err(message) => {
            eprintln!("[ERROR]: {message}");
            return Ok(1);
        }
    };
    // Held to the end of the run: the file the `ssh` calls ask for the password is removed
    // when it drops.
    let _connection_password = connection_password
        .map(|password| crate::askpass::install(&password))
        .transpose()?;
    let defaults = ConnectionDefaults {
        remote_user: args.user.clone().or(config.remote_user),
        connection: args.connection.clone(),
        private_key: args.private_key.clone().or(config.private_key_file),
        host_key_checking: config.host_key_checking,
        remote_tmp: config.remote_tmp,
        connect_timeout: args
            .timeout
            .map_or(config.timeout, std::time::Duration::from_secs),
        r#become: args.r#become || config.r#become,
        become_user: args
            .become_user
            .clone()
            .unwrap_or(config.become_user.clone()),
        become_method,
        become_exe: config.become_exe,
        become_flags: config.become_flags,
        become_password,
        ssh_common_args: ssh_words("--ssh-common-args", args.ssh_common_args.as_deref())?,
        ssh_extra_args: ssh_words("--ssh-extra-args", args.ssh_extra_args.as_deref())?,
    };
    let mut stats = Stats::default();

    let (stop_tx, stop_rx) = watch::channel(false);
    let abort = Arc::new(Abort::new(stop_tx));
    spawn_signal_watcher(Arc::clone(&abort));
    let options = RunOptions {
        defaults,
        forks,
        // The play's own keyword speaks over this; measured, the flag and `[defaults]
        // force_handlers` are the same request.
        force_handlers: args.force_handlers || config.force_handlers,
        batching: config.batching,
        control_dir: config
            .ssh_control_master
            .then(crate::transport::control_dir)
            .flatten(),
        stop: stop_rx.clone(),
        abort,
        reboots: Arc::default(),
        profile: Arc::clone(profile),
        step: args.step.then(Arc::default),
    };

    let playbook_dir = playbook::base_dir(&args.playbooks[0]);
    let cwd = std::env::current_dir()?;
    let extra = crate::vars::parse_extra_vars(&args.extra_vars, &cwd)?;
    // Kept for the local-agent check below, which runs before any host has an effective view
    // and would otherwise read `ansible_connection` from the inventory alone.
    let extra_for_check = extra.clone();
    let mut store = VarStore::new(&inventory, inventory_path.as_deref(), &playbook_dir, extra)?;
    store.set_forks(forks);
    // Where `setup` gets its facts, decided once for the whole run from everything its plays can
    // read. The run's native policy goes on whatever union came back, built or kept from an
    // earlier run: a union nobody armed keeps `Natives::default()`, which is natives off.
    let facts = match args.facts {
        Facts::Auto if config.native_modules => {
            let plays: Vec<_> = playbooks
                .iter()
                .zip(&compiled)
                .flat_map(|(pb, steps)| pb.plays.iter().zip(steps))
                .collect();
            let (facts, why) = facts_read::decide(&facts_read::facts_read(&plays, &reach, &store));
            profile.facts(why);
            facts
        }
        Facts::Auto => {
            profile.facts("python (native modules are off)".into());
            Facts::Python
        }
        forced => {
            profile.facts(format!("{forced:?} (--facts)").to_lowercase());
            forced
        }
    };
    let python = python.map(|union| armed(union, config.native_modules, facts));
    let (run_tags, skip_tags) = selection.lists();
    store.set_tags(run_tags, skip_tags);
    store.set_modes(args.check, args.diff);
    let mut state = RunState {
        templar: Arc::new(Templar::with_bridge(
            playbook_dir.clone(),
            Some(Arc::clone(&bridge)),
        )),
        vars: Arc::new(Mutex::new(store)),
        failed_hosts: HashSet::new(),
        unreachable_hosts: HashSet::new(),
        refreshed: None,
        verbosity: args.verbose,
        links: HashMap::new(),
        python: python.map(Arc::new),
    };

    // A controller with no local agent can still drive remote hosts, so the local agent only
    // has to be there once a play actually targets a host that runs one here. Saying so before
    // that play starts beats one `UNREACHABLE` per local host.
    //
    // The view this reads is the inventory's own variables under the run's extra variables, and
    // no more: a play's `vars:`, a `group_vars` file and a `set_fact` all reach a host's
    // connection later than this. A host those make local is checked when its link opens
    // instead, so this is an early word rather than a guarantee.
    let is_local = |host: &Host| {
        let mut vars: serde_json::Map<String, serde_json::Value> = host
            .vars
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (key, value) in &extra_for_check {
            vars.insert(key.clone(), value.clone());
        }
        matches!(
            Transport::for_vars(&host.name, &vars, &options.defaults),
            Ok(Transport::Local)
        )
    };
    let mut local_agent_checked = false;
    // The reference's display says a warning once for the run, however many plays repeat it.
    let mut prompt_warned = false;
    let mut current_dir = playbook_dir;
    // Set when a batch ended the run by losing every live host it had. The recap still prints,
    // measured, so it is printed once past the loop rather than inside it.
    let mut stopped = false;
    // Every way out of the loop below runs `shutdown_links` once, which is why the loop is a
    // block whose result is read afterwards rather than a stretch of `?`. `AgentLink::drop`
    // busy-polls for its agent for up to 200 ms, on the runtime thread and one link at a time,
    // so a `?` returning past the shutdown would pay that for every live link in series.
    // Before the first play starts: a play that cannot run is not found out after the ones in
    // front of it have.
    for (pb, steps) in playbooks.iter().zip(&compiled) {
        for (play, compiled) in pb.plays.iter().zip(steps) {
            if play.strategy.as_deref() == Some("host_pinned") {
                let hosts = play_hosts(&inventory, play, limit.as_ref(), out, &mut warned, false)?;
                executor::refuse_unsafe_pinning(play, compiled, hosts.len(), options.forks)?;
            }
        }
    }
    let plays: anyhow::Result<()> = async {
        'plays: for ((pb, steps), file) in playbooks.iter().zip(&compiled).zip(&args.playbooks) {
            out.playbook(file, pb.plays.len(), &dump);
            for (play, compiled) in pb.plays.iter().zip(steps) {
                if *stop_rx.borrow() {
                    break 'plays;
                }
                // Per play rather than per playbook argument: `import_playbook` splices another
                // file's plays in place, and each of them reads its `group_vars/`, its
                // `vars_files` and its lookups beside the file it was written in.
                if play.dir != current_dir {
                    state.templar = Arc::new(Templar::with_bridge(
                        play.dir.clone(),
                        Some(Arc::clone(&bridge)),
                    ));
                    state.vars.lock().expect("vars lock").rebase(&play.dir)?;
                    current_dir.clone_from(&play.dir);
                }
                // Asked before the banner and before the hosts are looked at, as the reference does.
                let play_vars = state.vars.lock().expect("vars lock").play_scope(&play.vars);
                let prompted = crate::prompt::prompted(
                    play,
                    &state.templar,
                    &play_vars,
                    &extra_for_check,
                    &mut prompt_warned,
                )?;
                let play = &*prompted;
                let hosts = play_hosts(&inventory, play, limit.as_ref(), out, &mut warned, true)?;
                if !local_agent_checked && hosts.iter().any(&is_local) {
                    agents.local()?;
                    local_agent_checked = true;
                }
                let end = executor::run_play(
                    play, compiled, hosts, &agents, &options, &mut state, out, &mut stats,
                )
                .await?;
                // `meta: refresh_inventory` read the file again: the plays behind this one
                // resolve their hosts against what it found.
                if let Some(fresh) = state.refreshed.take() {
                    inventory = fresh;
                }
                // `add_host` and `group_by` grew the inventory: the plays behind this one run on
                // what they added. Taken after the refresh, which the store already folded in.
                if let Some(grown) = state
                    .vars
                    .lock()
                    .expect("vars lock")
                    .take_inventory_changes()
                {
                    inventory = grown;
                }
                // A batch that lost every live host it had ends the whole run, measured: no
                // further play, and the recap prints straight away. The recap below belongs to
                // the playbook loop this leaves, so `stopped` carries it past the `break`.
                if end.stop_run {
                    stopped = true;
                    break 'plays;
                }
            }
            // One recap per playbook argument, as the reference prints it, and the counters carry
            // over: the second playbook's recap shows the whole run so far. An interrupted run
            // skips this one and prints its own below, so a stop never doubles the last recap.
            if *stop_rx.borrow() {
                break 'plays;
            }
            recap(out, &stats, &state, show_custom_stats);
        }
        Ok(())
    }
    .await;
    // Closing them here rather than letting the state drop keeps the wait for each agent off a
    // blocking drop inside the runtime.
    let closing = std::time::Instant::now();
    state.shutdown_links().await;
    profile.phase(None, Phase::Shutdown, micros(closing));
    plays?;
    if *stop_rx.borrow() {
        eprintln!("[ERROR]: User interrupted execution");
        recap(out, &stats, &state, show_custom_stats);
        return Ok(99);
    }
    if stopped {
        recap(out, &stats, &state, show_custom_stats);
    }
    if let Some(pattern) = args.start_at_task.as_ref().filter(|_| !start_found) {
        eprintln!(
            "[ERROR]: No matching task \"{pattern}\" found. Note: --start-at-task can only follow static includes."
        );
    }
    // `exit_code` reads the whole run, whatever the recaps showed along the way.
    Ok(exit_code(&stats))
}

/// `union` under the run's native policy: `[volant] native_modules` or `VOLANT_NATIVE_MODULES`,
/// and where `setup` gets its facts.
fn armed(union: python::Union, enabled: bool, facts: Facts) -> python::Union {
    python::Union {
        natives: Natives { enabled, facts },
        ..union
    }
}

/// The tags the run selects, from the configuration file, the environment and the command line.
///
/// Measured on ansible-core 2.19.12: `--tags` and `--skip-tags` **append** to the list
/// `[tags] run` or `ANSIBLE_RUN_TAGS` set, rather than replacing it, because the option's own
/// default is that list and `append` adds to a default. So `[tags] run = x` with `--tags y`
/// selects `x` and `y`. A value is split on commas and trimmed wherever it came from, so
/// `--tags x,y`, `--tags 'x, y'` and `--tags x --tags y` are the same three-word request.
///
/// `--list-tags` with nothing asked for is the one place the reference widens the selection
/// instead of narrowing it: measured, it lists `never` tags too, because the point of the
/// command is to show every tag a playbook has. A `--tags` or a `--skip-tags` from any source
/// takes that back.
fn tag_selection(args: &PlaybookArgs, config: &Config, mode: Listing) -> TagSelection {
    let joined = |from_config: &[String], from_cli: &[String]| -> Vec<String> {
        let mut tags = from_config.to_vec();
        tags.extend(
            from_cli
                .iter()
                .flat_map(|value| crate::config::tag_list(value)),
        );
        tags
    };
    let run = joined(&config.tags_run, &args.tags);
    let skip = joined(&config.tags_skip, &args.skip_tags);
    if run.is_empty() && skip.is_empty() && mode.tags {
        return TagSelection::new(vec!["never".to_string(), "all".to_string()], Vec::new());
    }
    TagSelection::new(run, skip)
}

/// The host names `--limit` narrows the whole run to, or `None` when it was not given.
///
/// Exit 1 when it leaves nothing, which is the reference's own code, measured - and measured
/// again for a listing: `--list-hosts --limit nosuch` exits 1 there too, printing nothing.
fn resolve_limit(
    args: &PlaybookArgs,
    inventory: &Inventory,
    out: &mut Renderer,
) -> anyhow::Result<Option<HashSet<String>>> {
    let Some(pattern) = &args.limit else {
        return Ok(None);
    };
    let resolution = inventory.resolve(pattern);
    // A `--limit` naming a host that is not there is a warning and not a refusal, the way it is
    // in a play's own `hosts`: measured, `-l web1,web-typo` warns `Could not match supplied host
    // pattern, ignoring: web-typo` and then runs on `web1`. Losing it meant a mistyped name
    // silently narrowed the run to nothing it was asked about.
    for unmatched in &resolution.unmatched {
        out.warning(
            &format!("Could not match supplied host pattern, ignoring: {unmatched}"),
            false,
        );
    }
    let names: HashSet<String> = resolution.hosts.into_iter().map(|h| h.name).collect();
    if names.is_empty() {
        anyhow::bail!(
            "Specified inventory, host pattern and/or --limit leaves us with no hosts to target."
        );
    }
    Ok(Some(names))
}

/// The hosts one play would run on: its own pattern resolved, narrowed by `--limit`. One
/// function for the run and for `--list-hosts`, so the listing cannot show a host list the run
/// would not use.
fn play_hosts(
    inventory: &Inventory,
    play: &playbook::Play,
    limit: Option<&HashSet<String>>,
    out: &mut Renderer,
    warned: &mut HashSet<String>,
    ordered: bool,
) -> anyhow::Result<Vec<Host>> {
    let resolution = inventory.resolve(&play.hosts);
    for warning in &resolution.warnings {
        if warned.insert(warning.clone()) {
            out.warning(warning, false);
        }
    }
    for pattern in &resolution.unmatched {
        out.warning(
            &format!("Could not match supplied host pattern, ignoring: {pattern}"),
            false,
        );
    }
    let mut hosts: Vec<Host> = match limit {
        Some(allowed) => resolution
            .hosts
            .into_iter()
            .filter(|h| allowed.contains(&h.name))
            .collect(),
        None => resolution.hosts,
    };
    if ordered {
        order_hosts(&mut hosts, play.order.as_deref())?;
    }
    Ok(hosts)
}

/// The play's `order`: the order its hosts run in, `inventory` when it says nothing. Exit 5 for
/// a value the reference does not know, which is its own code, measured, and it speaks before
/// the play's banner.
fn order_hosts(hosts: &mut [Host], order: Option<&str>) -> anyhow::Result<()> {
    match order.unwrap_or("inventory") {
        "inventory" => {}
        "reverse_inventory" => hosts.reverse(),
        "sorted" => hosts.sort_by(|a, b| a.name.cmp(&b.name)),
        "reverse_sorted" => hosts.sort_by(|a, b| b.name.cmp(&a.name)),
        "shuffle" => shuffle(hosts),
        other => {
            return Err(Refusal::at(
                5,
                format!("Invalid 'order' specified for inventory hosts: {other}"),
            ));
        }
    }
    Ok(())
}

/// A Fisher-Yates shuffle, unseeded like the reference's `random.shuffle`: the seed is the one
/// `RandomState` draws from the system for every hash map of the process.
fn shuffle<T>(items: &mut [T]) {
    use std::hash::{BuildHasher, Hasher};
    let state = std::collections::hash_map::RandomState::new();
    for i in (1..items.len()).rev() {
        let mut hasher = state.build_hasher();
        hasher.write_usize(i);
        items.swap(i, (hasher.finish() % (i as u64 + 1)) as usize);
    }
}

/// A password file as ansible-core's `get_password_from_file` reads it: `-` is standard
/// input, an executable file is a script whose output is the password, anything else is read
/// and stripped of surrounding whitespace. The wording is the reference's.
fn password_from_file(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let shown = path.display();
    let secret = if path == Path::new("-") {
        let mut bytes = Vec::new();
        std::io::stdin()
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Could not read password file '-': {e}"))?;
        bytes
    } else if !path.exists() {
        return Err(format!("The password file {shown} was not found"));
    } else if is_executable(path) {
        let out = std::process::Command::new(path)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| {
                format!(
                    "Problem occurred when trying to run the password script {shown} ({e}). If this is not a script, remove the executable bit from the file."
                )
            })?;
        if !out.status.success() {
            return Err(format!(
                "The password script {shown} returned an error (rc={}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        out.stdout
    } else {
        std::fs::read(path)
            .map_err(|_| format!("Could not read password file '{shown}'."))?
            .trim_ascii()
            .to_vec()
    };
    let end = secret
        .iter()
        .rposition(|b| !matches!(b, 13 | 10)) // CR, LF
        .map_or(0, |i| i + 1);
    let secret = &secret[..end];
    if secret.is_empty() {
        return Err(format!("Empty password was provided from file ({shown})"));
    }
    String::from_utf8(secret.to_vec())
        .map_err(|_| format!("The password file {shown} is not UTF-8"))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

/// The connection and the escalation password, asked in the reference's order. A prompt that
/// meets the end of the input ends the asking, and the run goes on with what it has, as
/// `ask_passwords` does when `getpass` raises `EOFError`. An empty escalation answer after `-k`
/// is the connection password.
fn ask_passwords(args: &PlaybookArgs) -> Result<(Option<String>, Option<String>), String> {
    let ask = |prompt: &str| -> Result<Option<String>, String> {
        let Some(typed) = crate::prompt::hidden_line(prompt).map_err(|e| format!("{e:#}"))? else {
            return Ok(None);
        };
        // `from_utf8` rather than `from_utf8_lossy`: a password quietly rewritten with
        // replacement characters would be refused by the host, and the run would blame the
        // operator.
        String::from_utf8(typed)
            .map(Some)
            .map_err(|_| "the password is not UTF-8".to_string())
    };
    let mut connection = None;
    let mut become_prompt = "BECOME password: ";
    if args.ask_pass {
        connection = ask("SSH password: ")?;
        if connection.is_none() {
            return Ok((None, None));
        }
        become_prompt = "BECOME password[defaults to SSH password]: ";
    } else if let Some(path) = &args.connection_password_file {
        connection = Some(password_from_file(path)?);
    }
    let mut escalation = None;
    if args.ask_become_pass {
        escalation = ask(become_prompt)?;
        if args.ask_pass && escalation.as_deref() == Some("") {
            escalation.clone_from(&connection);
        }
    } else if let Some(path) = &args.become_password_file {
        escalation = Some(password_from_file(path)?);
    }
    Ok((connection, escalation))
}

/// The words of `--ssh-common-args` or `--ssh-extra-args`; an unbalanced quote is refused
/// by name instead of connecting without an option the operator asked for.
fn ssh_words(flag: &str, text: Option<&str>) -> anyhow::Result<Vec<String>> {
    let Some(text) = text else {
        return Ok(Vec::new());
    };
    shlex::split(text).ok_or_else(|| {
        anyhow::anyhow!(
            "{flag} has unbalanced quotes and cannot be turned into ssh options: {text}"
        )
    })
}

/// Ctrl-C and SIGTERM both request a clean stop: running batches are cancelled, the recap is
/// printed, and the process exits with ansible-playbook's code 99.
fn spawn_signal_watcher(stop: Arc<Abort>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let Ok(mut term) = signal(SignalKind::terminate()) else {
                return;
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
        stop.interrupt();
    });
}

/// The recap, then what `set_stats` collected when the run asks to see it.
fn recap(out: &mut Renderer, stats: &Stats, state: &RunState, show_custom: bool) {
    out.recap(stats);
    if show_custom {
        out.custom_stats(state.vars.lock().expect("vars lock").custom_stats());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::testing::scratch;

    /// `--facts` takes one of its three words and nothing else, `auto` when it is not given, and
    /// `--profile` is a plain flag.
    #[test]
    fn facts_takes_one_of_three_words() {
        let parse = |extra: &[&str]| {
            PlaybookArgs::try_parse_from(["volant"].iter().chain(extra).chain(&["site.yml"]))
        };
        let args = parse(&[]).unwrap();
        assert_eq!((args.facts, args.profile), (Facts::Auto, false));
        let args = parse(&["--facts", "python", "--profile"]).unwrap();
        assert_eq!((args.facts, args.profile), (Facts::Python, true));
        assert_eq!(parse(&["--facts=native"]).unwrap().facts, Facts::Native);
        let err = parse(&["--facts", "nope"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue, "{err}");
    }

    /// `-M` and `--module-path` repeat and take colon lists; a later one goes in front, and the order
    /// inside one stays.
    #[rstest::rstest]
    #[case::one(&["-M", "/a"], &["/a"])]
    #[case::list(&["--module-path", "/a::/b"], &["/a", "/b"])]
    #[case::later_first(&["-M", "/a:/b", "-M", "/c"], &["/c", "/a", "/b"])]
    fn module_path_goes_in_front_in_the_reference_order(
        #[case] flags: &[&str],
        #[case] expected: &[&str],
    ) {
        let args =
            PlaybookArgs::try_parse_from(["volant"].iter().chain(flags).chain(&["site.yml"]))
                .unwrap();
        let dirs: Vec<String> = module_path_dirs(&args)
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        assert_eq!(dirs, expected);
    }

    /// The switch and `--facts` reach every payload the run sends, from the environment through the
    /// configuration to the union the tasks take their payloads from.
    #[test]
    fn the_native_switch_and_facts_reach_the_payloads() {
        let core = |name: &str| python::ModuleFacts {
            module_fqn: format!("ansible.modules.{name}"),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: serde_json::Map::new(),
            core: true,
        };
        let union = python::Union {
            hash: "ab".into(),
            zip_b64: "UEsDBA==".into(),
            modules: ["stat", "setup"]
                .into_iter()
                .map(|m| (m.to_string(), core(m)))
                .collect(),
            refused: std::collections::BTreeMap::default(),
            natives: Natives::default(),
        };
        let forced = |config: &Config, extra: &[&str], module: &str| {
            let args =
                PlaybookArgs::try_parse_from(["volant"].iter().chain(extra).chain(&["site.yml"]))
                    .unwrap();
            armed(union.clone(), config.native_modules, args.facts)
                .payload(module)
                .expect("the union holds it")
                .force_python
        };
        unsafe {
            std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
            std::env::remove_var("VOLANT_NATIVE_MODULES");
        }
        let on = Config::load().unwrap();
        assert!(!forced(&on, &[], "stat"));
        assert!(forced(&on, &[], "setup"), "--facts auto");
        assert!(forced(&on, &["--facts", "python"], "setup"));
        assert!(!forced(&on, &["--facts", "native"], "setup"));
        unsafe { std::env::set_var("VOLANT_NATIVE_MODULES", "0") };
        let off = Config::load().unwrap();
        assert!(forced(&off, &[], "stat"), "VOLANT_NATIVE_MODULES=0");
        assert!(forced(&off, &["--facts", "native"], "setup"));
    }

    /// Captures what `f` writes on this process's standard output, at the descriptor.
    #[cfg(unix)]
    fn stdout_of(f: impl FnOnce()) -> String {
        use std::io::Write as _;
        use std::os::fd::AsRawFd as _;
        let dir = scratch();
        let path = dir.path().join("stdout");
        let file = std::fs::File::create(&path).unwrap();
        std::io::stdout().flush().unwrap();
        let saved = unsafe { libc::dup(1) };
        assert!(saved >= 0);
        assert!(unsafe { libc::dup2(file.as_raw_fd(), 1) } >= 0);
        f();
        std::io::stdout().flush().unwrap();
        unsafe {
            libc::dup2(saved, 1);
            libc::close(saved);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        text
    }

    /// `--profile` adds nothing to standard output, and `VOLANT_PROFILE_JSON` gets the profile
    /// whatever became of the run.
    #[cfg(unix)]
    #[test]
    fn the_profile_stays_off_stdout_and_lands_in_its_file() {
        let dir_guard = scratch();
        let dir = dir_guard.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let playbook = dir.join("site.yml");
        std::fs::write(
            &playbook,
            "- hosts: nosuch
  gather_facts: false
  tasks:
    - debug: {msg: hi}
",
        )
        .unwrap();
        let json = dir.join("profile.jsonl");
        unsafe {
            std::env::set_var("ANSIBLE_CONFIG", "/nonexistent/volant/ansible.cfg");
            std::env::set_var("VOLANT_PROFILE_JSON", &json);
        }
        let playbook_arg = playbook.display().to_string();
        let args = |extra: &[&str]| {
            PlaybookArgs::try_parse_from(
                ["volant", "--no-color"]
                    .iter()
                    .chain(extra)
                    .chain(&[playbook_arg.as_str()]),
            )
            .unwrap()
        };
        let plain = stdout_of(|| assert_eq!(run(args(&[])), 0));
        let profiled = stdout_of(|| assert_eq!(run(args(&["--profile"])), 0));
        assert!(plain.contains("PLAY RECAP"), "{plain}");
        assert_eq!(profiled, plain);
        let first = std::fs::read_to_string(&json).unwrap();
        assert_eq!(first.lines().next(), Some(r#"{"natives":{}}"#), "{first}");

        std::fs::remove_file(&json).unwrap();
        let missing = dir.join("missing.yml").display().to_string();
        let args = PlaybookArgs::try_parse_from(["volant", missing.as_str()]).unwrap();
        assert_ne!(run(args), 0);
        let written = std::fs::read_to_string(&json).unwrap();
        assert_eq!(
            written.lines().next(),
            Some(r#"{"natives":{}}"#),
            "{written}"
        );
    }

    /// A password file gives its whole text, surrounding whitespace removed; a missing or
    /// empty one is refused in the reference's words.
    #[rstest::rstest]
    #[case::newline(
        Some(
            "sekret
"
        ),
        Ok("sekret")
    )]
    #[case::no_newline(Some("sekret"), Ok("sekret"))]
    #[case::inner_space(
        Some(
            "sek ret 

"
        ),
        Ok("sek ret")
    )]
    #[case::leading_newline(
        Some(
            "
sekret
"
        ),
        Ok("sekret")
    )]
    #[case::several_lines(
        Some(
            "one
two
"
        ),
        Ok(
            "one
two"
        )
    )]
    #[case::crlf(
        Some(
            "sekret
"
        ),
        Ok("sekret")
    )]
    #[case::empty(Some(""), Err("Empty password was provided from file (<path>)"))]
    #[case::newlines_only(
        Some(
            "

"
        ),
        Err("Empty password was provided from file (<path>)")
    )]
    #[case::spaces_only(
        Some(
            "  
"
        ),
        Err("Empty password was provided from file (<path>)")
    )]
    #[case::missing(None, Err("The password file <path> was not found"))]
    fn a_password_file_gives_its_text(
        #[case] content: Option<&str>,
        #[case] want: Result<&str, &str>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pw");
        if let Some(content) = content {
            std::fs::write(&path, content).unwrap();
        }
        let got = password_from_file(&path);
        let want = want
            .map(str::to_string)
            .map_err(|m| m.replace("<path>", &path.display().to_string()));
        assert_eq!(got, want);
    }

    /// A directory exists and cannot be read; an executable file gives its output, or its
    /// failure.
    #[cfg(unix)]
    #[test]
    fn a_directory_and_a_script_as_password_files() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            password_from_file(dir.path()),
            Err(format!(
                "Could not read password file '{}'.",
                dir.path().display()
            ))
        );
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        assert_eq!(
            password_from_file(&script(
                "ok",
                "#!/bin/sh
printf 'a b \n'
"
            )),
            Ok("a b ".to_string())
        );
        let failing = script(
            "bad",
            "#!/bin/sh
echo nope >&2
exit 3
",
        );
        assert_eq!(
            password_from_file(&failing),
            Err(format!(
                "The password script {} returned an error (rc=3): nope
",
                failing.display()
            ))
        );
    }

    /// The prompt and the file for one password are exclusive, and `ssh` arguments with an
    /// open quote are refused by name.
    #[rstest::rstest]
    #[case::ask_and_file(&["-k", "--connection-password-file", "f"], false)]
    #[case::alias(&["-k", "--conn-pass-file", "f"], false)]
    #[case::become_ask_and_file(&["-K", "--become-password-file", "f"], false)]
    #[case::become_alias(&["-K", "--become-pass-file", "f"], false)]
    #[case::one_each(&["-k", "-K"], true)]
    #[case::files(&["--conn-pass-file", "f", "--become-pass-file", "g"], true)]
    fn password_options_exclude_each_other(#[case] flags: &[&str], #[case] parses: bool) {
        let argv = ["volant"]
            .into_iter()
            .chain(flags.iter().copied())
            .chain(["site.yml"]);
        assert_eq!(PlaybookArgs::try_parse_from(argv).is_ok(), parses);
        assert!(ssh_words("--ssh-common-args", Some("-o 'ProxyJump=a")).is_err());
        assert_eq!(
            ssh_words("--ssh-extra-args", Some("-o A=1 -v")).unwrap(),
            ["-o", "A=1", "-v"]
        );
    }

    /// `order` arranges a play's hosts, `shuffle` keeps every one of them, and a value the
    /// reference does not know is refused with its own exit code.
    #[rstest::rstest]
    #[case::inventory(None, Ok(vec!["hc", "ha", "hb"]))]
    #[case::inventory_named(Some("inventory"), Ok(vec!["hc", "ha", "hb"]))]
    #[case::reverse_inventory(Some("reverse_inventory"), Ok(vec!["hb", "ha", "hc"]))]
    #[case::sorted(Some("sorted"), Ok(vec!["ha", "hb", "hc"]))]
    #[case::reverse_sorted(Some("reverse_sorted"), Ok(vec!["hc", "hb", "ha"]))]
    #[case::bogus(
        Some("bogus"),
        Err("Invalid 'order' specified for inventory hosts: bogus")
    )]
    fn order_arranges_the_hosts(
        #[case] order: Option<&str>,
        #[case] want: Result<Vec<&str>, &str>,
    ) {
        let mut hosts: Vec<Host> = ["hc", "ha", "hb"]
            .iter()
            .map(|name| Host {
                name: (*name).to_string(),
                vars: std::collections::BTreeMap::new(),
            })
            .collect();
        let got = order_hosts(&mut hosts, order);
        match want {
            Ok(names) => {
                got.expect("the order is known");
                assert_eq!(
                    hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
                    names
                );
            }
            Err(message) => {
                let err = got.expect_err("the order is refused");
                assert_eq!(error_code(&err), 5);
                assert!(format!("{err:#}").contains(message), "{err:#}");
            }
        }
    }

    #[test]
    fn shuffle_keeps_every_host() {
        let mut hosts: Vec<u32> = (0..50).collect();
        shuffle(&mut hosts);
        hosts.sort_unstable();
        assert_eq!(hosts, (0..50).collect::<Vec<_>>());
    }
}
