// SPDX-License-Identifier: GPL-3.0-or-later
//! `volant-playbook`: the same as `volant playbook`, so CI scripts change one word.

use clap::Parser;
use volant::cli::PlaybookArgs;

#[derive(Parser)]
#[command(name = "volant-playbook")]
struct Cli {
    #[command(flatten)]
    args: PlaybookArgs,
}

fn main() {
    volant::askpass::answer_if_asked();
    std::process::exit(volant::cli::run(Cli::parse().args));
}
