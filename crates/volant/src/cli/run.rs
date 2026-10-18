// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anstream::ColorChoice;
use tokio::sync::watch;

use super::announce::{
    announce, announce_callbacks, inventory_sources, module_path_dirs, options_dump, version_line,
};
use super::args::PlaybookArgs;
use super::hosts::{play_hosts, resolve_limit, tag_selection};
use super::passwords::{ask_passwords, ssh_words};
use crate::bridge::Bridge;
use crate::compile::{self, Compiled};
use crate::config::Config;
use crate::executor::{self, Abort, RunOptions, RunState};
use crate::inventory::{Host, Note, load_sources_with};
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
    crate::transport::trace::configure(args.verbose, choice);
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
            match err.downcast_ref::<Refusal>() {
                Some(r) if r.plain => eprintln!("[ERROR]: {}", r.message),
                _ => eprintln!("ERROR! {err:#}"),
            }
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

#[expect(clippy::too_many_lines)]
async fn run_all(
    args: &PlaybookArgs,
    out: &mut Renderer,
    profile: &Arc<Profile>,
) -> anyhow::Result<i32> {
    let config = crate::runtime::resolve::configured(Config::load()?);
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
    let sources = inventory_sources(args, &config);
    // One helper for the run: the inventory's collection plugins, its refreshes, and the filters.
    let bridge = Bridge::late();
    let loaded = load_sources_with(&sources, &config.inventory_plugins, &|| Arc::clone(&bridge));
    for note in &loaded.notes {
        match note {
            Note::Verbose(level, text) => out.verbose(*level, text),
            Note::Warning(text) => out.warning(text, false),
            Note::Deprecation(text) => playbook::print_deprecation(text),
        }
    }
    if let Some(fatal) = loaded.error {
        if fatal.code == 4 {
            return Err(Refusal::at(4, fatal.message));
        }
        eprintln!("[ERROR]: {}", fatal.message);
        return Ok(fatal.code);
    }
    let mut inventory = loaded.inventory;
    announce_callbacks(out);
    let dump = if out.verbose_at(4) {
        options_dump(args, &config, forks)
    } else {
        Vec::new()
    };
    // The cache directory is made when the plugin loads, before any playbook is read.
    let (cache, cache_warnings) = crate::fact_cache::FactCache::open(&config);
    for warning in &cache_warnings {
        out.warning(warning, false);
    }
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
        // `gathering = explicit` gathers for the plays that ask and for no other.
        if config.gathering == "explicit" && !play.gather_written {
            play.gather_facts = false;
        }
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
    bridge
        .set_plugin_dirs(reach.plugin_dirs.clone())
        .map_err(|e| Refusal::or(4, e))?;
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
    let mut store = VarStore::with_sources(&inventory, &sources, &playbook_dir, extra)?;
    store.set_forks(forks);
    store.set_reload(
        sources.clone(),
        config.inventory_plugins.clone(),
        Arc::clone(&bridge),
        limit.clone(),
    );
    if let Some(cache) = &cache {
        let names: Vec<String> = inventory
            .resolve("all")
            .hosts
            .into_iter()
            .map(|h| h.name)
            .collect();
        if args.flush_cache {
            cache
                .flush(names.iter().map(String::as_str))
                .map_err(|e| anyhow::anyhow!("Unable to flush the fact cache: {e}"))?;
        }
        for name in &names {
            match cache.read(name) {
                Ok(Some(facts)) => store.load_cached(name, facts),
                Ok(None) => {}
                // Reported once the play's banner is out, as the reference does.
                Err(failure) => {
                    store.fail_cache(failure);
                    break;
                }
            }
        }
    }
    store.set_fact_cache(cache, config.gathering == "smart");
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
                state.vars.lock().expect("vars lock").end_play();
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
pub(super) fn armed(union: python::Union, enabled: bool, facts: Facts) -> python::Union {
    python::Union {
        natives: Natives { enabled, facts },
        ..union
    }
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
