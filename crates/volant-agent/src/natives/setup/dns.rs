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
mod tests {
    use serde_json::json;

    use super::super::tests::{FakeRoot, probe};
    use super::*;

    /// A systemd-resolved stub file, sanitised, plus the quirks: servers accumulate, a later
    /// `search` replaces an earlier one, an option with an empty value is `true`.
    #[test]
    fn resolv_conf_is_read_like_the_reference() {
        let fake = FakeRoot::new("dns");
        let (root, probe) = (fake.root(), probe());
        assert_eq!(collect(&fake.host(&root, &probe))["dns"], json!({}));
        fake.write(
            "/etc/resolv.conf",
            "# comment\n; other\nnameserver 127.0.0.53\nnameserver 10.0.0.1 10.0.0.2\n\
             options edns0 trust-ad ndots:2 timeout:\nsearch first\nsearch example home\n\
             domain\ndomain example\n",
        );
        assert_eq!(
            collect(&fake.host(&root, &probe))["dns"],
            json!({
                "nameservers": ["127.0.0.53", "10.0.0.1", "10.0.0.2"],
                "options": {"edns0": true, "trust-ad": true, "ndots": "2", "timeout": true},
                "search": ["example", "home"],
                "domain": "example",
            })
        );
    }
}
