// SPDX-License-Identifier: GPL-3.0-or-later
//! What every command that runs against an inventory does first.

use std::sync::Arc;

use super::announce::{announce, inventory_sources, module_path_dirs};
use super::args::PlaybookArgs;
use crate::bridge::Bridge;
use crate::config::Config;
use crate::inventory::{Inventory, Note, Source, load_sources_with};
use crate::render::Renderer;
use crate::stats::Refusal;
use crate::{playbook, python};

/// What a command that runs against an inventory sets up before it reads anything else, in the
/// reference's order: the configuration and the colour, the `-v` lines, the module path, the
/// vault secrets, the forks and the inventory with its notes.
pub(super) struct SetUp {
    pub config: Config,
    pub forks: usize,
    pub sources: Vec<Source>,
    /// The run's one helper: the inventory's collection plugins, its refreshes, and the filters.
    pub bridge: Arc<Bridge>,
    pub inventory: Inventory,
}

/// [`SetUp`] for `args`. `Ok(Err(code))` is a refusal already printed, which ends the command
/// with `code`.
pub(super) fn set_up(
    args: &PlaybookArgs,
    out: &mut Renderer,
) -> anyhow::Result<Result<SetUp, i32>> {
    let config = crate::runtime::resolve::configured(Config::load()?);
    out.set_path_on_failure(config.show_task_path_on_failure);
    playbook::set_deprecation_warnings(config.deprecation_warnings);
    for notice in &args.adhoc_deprecations {
        playbook::print_deprecation_line(notice);
    }
    let choice = config.color_choice(args.no_color);
    out.set_choice(choice);
    crate::transport::trace::configure(args.verbose, choice);
    python::set_module_path(module_path_dirs(args));
    // A console's command: the session announced itself and opened the vault when it started.
    if args.carried.is_some() {
        return load_inventory(args, out, config);
    }
    announce(out, args.adhoc_pattern.as_ref().map(|_| args));
    let secrets = match crate::vault::load(&crate::vault::Request {
        ask: args.vault.ask_vault_pass,
        password_files: args.vault.vault_password_file.clone(),
        ids: args.vault.vault_id.clone(),
        config_file: config.vault_password_file.clone(),
        config_ids: config.vault_identity_list.clone(),
    }) {
        Ok(secrets) => secrets,
        Err(err) => {
            eprintln!("[WARNING]: {}", err.warning);
            eprintln!("[ERROR]: {}", err.message);
            return Ok(Err(1));
        }
    };
    crate::vault::install(secrets);
    load_inventory(args, out, config)
}

/// The forks and the inventory with its notes.
fn load_inventory(
    args: &PlaybookArgs,
    out: &mut Renderer,
    config: Config,
) -> anyhow::Result<Result<SetUp, i32>> {
    // Refused before anything is loaded, the way the reference refuses it, whether it comes
    // from the command line, the environment or `ansible.cfg`. Exit 2: `Cli::parse()` already
    // exits 2 for `-f abc` or `-f -1` (clap's own default for a bad argument), so a refused `0`
    // through this check stays on the same code rather than inventing a second one for the same
    // flag; a refusal never prints a `PLAY` header or a recap, so it cannot be mistaken for a
    // failed task, which is exit 2's other meaning.
    let forks = args.forks.unwrap_or(config.forks);
    if forks == 0 {
        // The ad hoc command's parser refuses it, whatever supplied the number.
        if args.adhoc_pattern.is_some() {
            return Ok(Err(super::adhoc_usage::refuse(
                "The number of processes (--forks) must be >= 1",
                false,
            )));
        }
        eprintln!("ERROR! The number of processes (--forks) must be >= 1");
        return Ok(Err(2));
    }
    let sources = inventory_sources(&args.hosts.inventory, &config);
    let bridge = Bridge::late();
    let loaded = load_sources_with(&sources, &config.inventory_plugins, &|| Arc::clone(&bridge));
    for note in &loaded.notes {
        match note {
            Note::Verbose(level, text) => out.verbose(*level, text),
            Note::Warning(text) => out.warning(text, false),
            Note::Error(text) => eprintln!("[ERROR]: {text}"),
            Note::Deprecation(text) => playbook::print_deprecation(text),
        }
    }
    if let Some(fatal) = loaded.error {
        if fatal.code == 4 {
            return Err(Refusal::at(4, fatal.message));
        }
        eprintln!("[ERROR]: {}", fatal.message);
        return Ok(Err(fatal.code));
    }
    let inventory = loaded.inventory;
    // The reference checks the whole inventory, before any `--limit` and any playbook, and the
    // implicit `localhost` is not in `all`.
    // The ad hoc command skips it for the pattern that names the implicit localhost.
    let names_localhost = args
        .adhoc_pattern
        .as_deref()
        .is_some_and(|pattern| ["127.0.0.1", "localhost", "::1"].contains(&pattern));
    if config.localhost_warning && !names_localhost && inventory.resolve("all").hosts.is_empty() {
        out.warning(
            "provided hosts list is empty, only localhost is available. Note that the implicit localhost does not match 'all'",
            false,
        );
    }
    Ok(Ok(SetUp {
        config,
        forks,
        sources,
        bridge,
        inventory,
    }))
}
