// SPDX-License-Identifier: GPL-3.0-or-later
use clap::Parser;

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("VOLANT_GIT_SHA"),
    " ",
    env!("VOLANT_BUILD_DATE"),
    ")"
);

/// Fast, drop-in engine for Ansible playbooks.
#[derive(Parser)]
#[command(name = "volant", version = VERSION)]
struct Cli {}

fn main() {
    Cli::parse();
}
