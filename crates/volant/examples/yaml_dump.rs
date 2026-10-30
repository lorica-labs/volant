// SPDX-License-Identifier: GPL-3.0-or-later
//! Reads YAML file paths on standard input, one per line, and prints one JSON line per file: its
//! documents as Volant's loader reads them, Volant's own encodings undone, or the error. `just
//! yaml-corpus` compares the lines with what ansible-core's loader returns.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() -> std::io::Result<()> {
    // The comparison counts duplicate keys itself; the warnings would only fill the terminal.
    volant::yaml::set_duplicate_keys("ignore");
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for path in std::io::stdin().lock().lines() {
        let path = path?;
        let line = match dump(&path) {
            Ok(docs) => json!({"path": path, "docs": docs}),
            Err(error) => json!({"path": path, "error": error}),
        };
        writeln!(out, "{line}")?;
    }
    out.flush()
}

fn dump(path: &str) -> Result<Vec<Value>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    let docs = volant::yaml::load(&text, path).map_err(|e| format!("{e:#}"))?;
    docs.iter()
        .map(|doc| {
            let mut value = volant::yaml::to_json(doc).map_err(|e| format!("{e:#}"))?;
            volant::yaml::as_reference(&mut value);
            Ok(value)
        })
        .collect()
}
