// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{Parser, Subcommand};
use volant::cli::{
    AdhocArgs, ConsoleArgs, GalaxyArgs, InventoryArgs, PlaybookArgs, PullArgs, RuntimeCommand,
    ShimsCommand, ToolArgs, VaultArgs,
};

/// Fast engine for Ansible playbooks.
#[derive(Parser)]
#[command(name = "volant", version = volant::VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run playbooks, with the arguments of ansible-playbook.
    Playbook(Box<PlaybookArgs>),
    /// Run one module against the hosts of a pattern, with the arguments of ansible.
    Adhoc(Box<AdhocArgs>),
    /// Browse plugin documentation, with the arguments of ansible-doc.
    Doc(ToolArgs),
    /// Show and validate the configuration, with the arguments of ansible-config.
    Config(ToolArgs),
    /// Install and list roles and collections, with the arguments of ansible-galaxy.
    Galaxy(Box<GalaxyArgs>),
    /// Run ad hoc commands in an interactive shell, with the arguments of ansible-console.
    Console(Box<ConsoleArgs>),
    /// Clone a repository and run its playbook, with the arguments of ansible-pull.
    Pull(Box<PullArgs>),
    /// Print an inventory as JSON, with the arguments of ansible-inventory.
    Inventory(InventoryArgs),
    /// Install, show or remove the Python runtime Volant manages for ansible-core.
    #[command(subcommand)]
    Runtime(RuntimeCommand),
    /// Link the ansible-core program names to volant, or remove those links.
    #[command(subcommand)]
    Shims(ShimsCommand),
    /// Encrypt, decrypt and view vault files, with the arguments of ansible-vault.
    Vault(VaultArgs),
    /// Replay a run the volant.compat.record callback recorded and report what Volant does
    /// differently.
    #[command(subcommand)]
    Shadow(volant::shadow::ShadowCommand),
}

/// Parses the command line. Started under an ansible-core program name, the process behaves as
/// the matching subcommand and its usage errors name the program as started.
fn parse() -> Cli {
    #[cfg(unix)]
    {
        use clap::{CommandFactory, FromArgMatches};
        use std::ffi::OsString;

        let mut argv = std::env::args_os();
        let program = argv.next().unwrap_or_default();
        if let Some(sub) = volant::cli::subcommand_for(&program) {
            let started = std::path::Path::new(&program)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let args = [OsString::from("volant"), OsString::from(sub)]
                .into_iter()
                .chain(argv);
            let parsed = Cli::command()
                .bin_name(started.clone())
                .try_get_matches_from(args)
                .and_then(|matches| Cli::from_arg_matches(&matches));
            return parsed.unwrap_or_else(|err| {
                if sub == "adhoc"
                    && let Some(code) = volant::cli::adhoc_usage_error(&err)
                {
                    std::process::exit(code);
                }
                // clap names a subcommand's usage after its parent, `ansible-playbook playbook`.
                let text = err
                    .render()
                    .to_string()
                    .replace(&format!("{started} {sub}"), &started);
                if err.use_stderr() {
                    eprint!("{started}: {text}");
                } else {
                    print!("{text}");
                }
                std::process::exit(err.exit_code());
            });
        }
    }
    Cli::try_parse().unwrap_or_else(|err| {
        if std::env::args_os().nth(1).is_some_and(|arg| arg == "adhoc")
            && let Some(code) = volant::cli::adhoc_usage_error(&err)
        {
            std::process::exit(code);
        }
        err.exit()
    })
}

fn main() {
    volant::askpass::answer_if_asked();
    let code = match parse().command {
        Command::Playbook(args) => volant::cli::run(*args),
        Command::Adhoc(args) => volant::cli::adhoc(*args),
        Command::Doc(args) => volant::cli::doc(args),
        Command::Config(args) => volant::cli::config(args),
        Command::Galaxy(args) => volant::cli::galaxy(*args),
        Command::Console(args) => volant::cli::console(*args),
        Command::Pull(args) => volant::cli::pull(*args),
        Command::Inventory(args) => volant::cli::inventory(args),
        Command::Runtime(command) => volant::cli::runtime(command),
        Command::Shims(command) => volant::cli::shims(command),
        Command::Vault(args) => volant::cli::vault(args),
        Command::Shadow(command) => volant::shadow::shadow(command),
    };
    std::process::exit(code);
}
