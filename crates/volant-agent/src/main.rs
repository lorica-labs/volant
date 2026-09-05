// SPDX-License-Identifier: GPL-3.0-or-later
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("volant-agent {}", env!("CARGO_PKG_VERSION"));
    }
}
