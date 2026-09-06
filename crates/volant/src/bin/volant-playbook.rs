// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant-playbook`: the same as `volant playbook`, so CI scripts change one word.

use clap::Parser;
use volant::cli::PlaybookArgs;

#[derive(Parser)]
#[command(name = "volant-playbook", version = volant::VERSION)]
struct Cli {
    #[command(flatten)]
    args: PlaybookArgs,
}

fn main() {
    std::process::exit(volant::cli::run(Cli::parse().args));
}
