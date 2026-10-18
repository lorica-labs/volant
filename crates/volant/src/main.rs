// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{Parser, Subcommand};
use volant::cli::{PlaybookArgs, RuntimeCommand};

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
    /// Install, show or remove the Python runtime Volant manages for ansible-core.
    #[command(subcommand)]
    Runtime(RuntimeCommand),
}

fn main() {
    volant::askpass::answer_if_asked();
    let code = match Cli::parse().command {
        Command::Playbook(args) => volant::cli::run(*args),
        Command::Runtime(command) => volant::cli::runtime(command),
    };
    std::process::exit(code);
}
