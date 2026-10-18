// SPDX-License-Identifier: GPL-3.0-or-later
//! The `ssh_pub_keys` collector: the host keys of the first directory that has some, in the
//! reference's loop, which stops at the first algorithm already found.

use serde_json::{Map, Value};

use super::Host;
use crate::natives::runtime::py_split;

pub fn collect(host: &Host) -> Result<Map<String, Value>, String> {
    let mut facts = Map::new();
    for dir in ["/etc/ssh", "/etc/openssh", "/etc"] {
        for algo in ["dsa", "rsa", "ecdsa", "ed25519"] {
            let name = format!("ssh_host_key_{algo}_public");
            if facts.contains_key(&name) {
                return Ok(facts);
            }
            let Some(data) = host.root.content(&format!("{dir}/ssh_host_{algo}_key.pub")) else {
                continue;
            };
            let mut words = py_split(&data);
            // The reference unpacks two words, and one word fails the whole collector.
            let (Some(keytype), Some(key)) = (words.next(), words.next()) else {
                return Err(format!("{dir}/ssh_host_{algo}_key.pub is not a public key"));
            };
            facts.insert(name.clone(), Value::from(key));
            facts.insert(format!("{name}_keytype"), Value::from(keytype));
        }
    }
    Ok(facts)
}

#[cfg(test)]
mod tests;
