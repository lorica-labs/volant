// SPDX-License-Identifier: GPL-3.0-or-later
//! The `dns` collector: `/etc/resolv.conf`, read line by line as `DnsFactCollector` reads it.

use serde_json::{Map, Value};

use super::{Host, py_split, py_strip, splitlines};

pub fn collect(host: &Host) -> Map<String, Value> {
    let content = host.root.content("/etc/resolv.conf").unwrap_or_default();
    let mut dns = Map::new();
    for line in splitlines(&content) {
        if line.starts_with('#') || line.starts_with(';') || py_strip(line).is_empty() {
            continue;
        }
        let tokens: Vec<&str> = py_split(line).collect();
        let words = || tokens[1..].iter().map(|word| Value::from(*word));
        match tokens[0] {
            "nameserver" => {
                let servers = dns
                    .entry("nameservers")
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(servers) = servers {
                    servers.extend(words());
                }
            }
            "domain" => {
                if let Some(domain) = tokens.get(1) {
                    dns.insert("domain".into(), Value::from(*domain));
                }
            }
            "search" => {
                dns.insert("search".into(), Value::Array(words().collect()));
            }
            "sortlist" => {
                dns.insert("sortlist".into(), Value::Array(words().collect()));
            }
            "options" => {
                let options = tokens[1..]
                    .iter()
                    .map(|option| match option.split_once(':') {
                        Some((name, value)) if !value.is_empty() => {
                            (name.to_string(), Value::from(value))
                        }
                        Some((name, _)) => (name.to_string(), Value::Bool(true)),
                        None => ((*option).to_string(), Value::Bool(true)),
                    })
                    .collect();
                dns.insert("options".into(), Value::Object(options));
            }
            _ => {}
        }
    }
    let mut facts = Map::new();
    facts.insert("dns".into(), Value::Object(dns));
    facts
}

#[cfg(test)]
mod tests;
