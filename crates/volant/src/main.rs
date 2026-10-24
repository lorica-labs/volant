// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{Parser, Subcommand};
use volant::cli::{PlaybookArgs, RuntimeCommand, ShimsCommand};

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
    /// Link the ansible-core program names to volant, or remove those links.
    #[command(subcommand)]
    Shims(ShimsCommand),
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
    Cli::parse()
}

fn main() {
    volant::askpass::answer_if_asked();
    let code = match parse().command {
        Command::Playbook(args) => volant::cli::run(*args),
        Command::Runtime(command) => volant::cli::runtime(command),
        Command::Shims(command) => volant::cli::shims(command),
    };
    std::process::exit(code);
}
