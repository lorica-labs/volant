// SPDX-License-Identifier: GPL-3.0-or-later
//! The `cmdline` collector: `/proc/cmdline` split by `shlex.split(data, posix=False)`, once with
//! the last value of a repeated key winning (`cmdline`) and once collecting them (`proc_cmdline`).

use serde_json::{Map, Value};

use super::Host;

pub fn collect(host: &Host) -> Map<String, Value> {
    let mut facts = Map::new();
    let Some(data) = host.root.content("/proc/cmdline") else {
        return facts;
    };
    let pieces = split_non_posix(&data).unwrap_or_default();
    let mut last = Map::new();
    let mut all = Map::new();
    for piece in &pieces {
        let (key, value) = match piece.split_once('=') {
            Some((key, value)) => (key, Value::from(value)),
            None => (piece.as_str(), Value::Bool(true)),
        };
        last.insert(key.to_string(), value.clone());
        match (all.get_mut(key), value) {
            (_, Value::Bool(true)) => {
                all.insert(key.to_string(), Value::Bool(true));
            }
            (Some(Value::Array(values)), value) => values.push(value),
            (Some(previous), value) => *previous = Value::Array(vec![previous.clone(), value]),
            (None, value) => {
                all.insert(key.to_string(), value);
            }
        }
    }
    facts.insert("cmdline".into(), Value::Object(last));
    facts.insert("proc_cmdline".into(), Value::Object(all));
    facts
}

/// `shlex.split(data, posix=False)`: words split on whitespace; a word that starts with a quote
/// runs to the matching quote, keeps both, and ends there. `None` for an unclosed quote, where
/// the reference's `ValueError` leaves both facts empty.
fn split_non_posix(data: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = data.chars();
    let mut word = String::new();
    while let Some(c) = chars.next() {
        if matches!(c, ' ' | '\t' | '\r' | '\n') {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else if word.is_empty() && matches!(c, '"' | '\'') {
            word.push(c);
            loop {
                let next = chars.next()?;
                word.push(next);
                if next == c {
                    break;
                }
            }
            words.push(std::mem::take(&mut word));
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::tests::{FakeRoot, probe};
    use super::*;

    /// A kernel command line with a repeated key, a bare flag, quotes kept by the non-POSIX
    /// split and a quoted word that ends where its quote closes.
    ///
    /// What would make this red: a POSIX split (quotes removed, `"a b"` one word), the last
    /// repeated value dropped from `proc_cmdline`, or the flag spelled as a string.
    #[test]
    fn the_kernel_command_line_is_split_like_the_reference() {
        let fake = FakeRoot::new("cmdline");
        let (root, probe) = (fake.root(), probe());
        assert!(
            collect(&fake.host(&root, &probe)).is_empty(),
            "no file, no keys"
        );
        fake.write(
            "/proc/cmdline",
            "BOOT_IMAGE=/vmlinuz-6.8.0 root=/dev/mapper/vg-root ro console=tty0 console=ttyS0,115200 x=\"a b\" \"q r\"s\n",
        );
        let facts = collect(&fake.host(&root, &probe));
        assert_eq!(
            facts["cmdline"],
            json!({"BOOT_IMAGE": "/vmlinuz-6.8.0", "root": "/dev/mapper/vg-root", "ro": true,
                   "console": "ttyS0,115200", "x": "\"a", "b\"": true, "\"q r\"": true, "s": true})
        );
        assert_eq!(
            facts["proc_cmdline"]["console"],
            json!(["tty0", "ttyS0,115200"])
        );
        fake.write("/proc/cmdline", "a=1 \"unclosed\n");
        let facts = collect(&fake.host(&root, &probe));
        assert_eq!(facts["cmdline"], json!({}));
        assert_eq!(facts["proc_cmdline"], json!({}));
    }
}
