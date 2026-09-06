// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{Parser, Subcommand};
use volant::cli::PlaybookArgs;

/// Fast, drop-in engine for Ansible playbooks.
#[derive(Parser)]
#[command(name = "volant", version = volant::VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run playbooks, with the arguments of ansible-playbook.
    Playbook(PlaybookArgs),
}

fn main() {
    let code = match Cli::parse().command {
        Command::Playbook(args) => volant::cli::run(args),
    };
    std::process::exit(code);
}
