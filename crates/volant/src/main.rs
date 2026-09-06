// SPDX-License-Identifier: GPL-3.0-or-later
use clap::Parser;

/// Fast, drop-in engine for Ansible playbooks.
#[derive(Parser)]
#[command(name = "volant", version = volant::VERSION)]
struct Cli {}

fn main() {
    Cli::parse();
}
