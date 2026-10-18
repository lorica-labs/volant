// SPDX-License-Identifier: GPL-3.0-or-later
//! The `user` collector: `getpass.getuser()`, which reads the environment before the password
//! database, and that user's entry. The entry is read from `/etc/passwd` when the name service
//! looks there first; a user only another service knows hands back.

use serde_json::{Map, Value};

use super::Host;
use crate::natives::runtime::py_split;

pub fn collect(host: &Host) -> Result<Map<String, Value>, String> {
    let (uid, euid, gid) = unsafe { (libc::getuid(), libc::geteuid(), libc::getgid()) };
    collect_as(host, uid, euid, gid)
}

fn collect_as(host: &Host, uid: u32, euid: u32, gid: u32) -> Result<Map<String, Value>, String> {
    let first = host
        .root
        .content("/etc/nsswitch.conf")
        .and_then(|conf| {
            conf.lines().find_map(|line| {
                line.trim_start()
                    .strip_prefix("passwd:")
                    .map(str::to_string)
            })
        })
        .and_then(|sources| py_split(&sources).next().map(str::to_string));
    if !matches!(first.as_deref(), Some("files" | "compat")) {
        return Err("the password database is not read from /etc/passwd first".into());
    }
    let passwd = std::fs::read_to_string(host.root.path("/etc/passwd"))
        .map_err(|err| format!("reading /etc/passwd: {err}"))?;
    let mut entries = Vec::new();
    for line in passwd.lines() {
        if line.starts_with('+') || line.starts_with('-') {
            return Err("/etc/passwd has NIS entries".into());
        }
        let fields: Vec<&str> = line.split(':').collect();
        if let [name, _, uid, gid, gecos, dir, shell] = fields[..]
            && let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>())
        {
            entries.push((name, uid, gid, gecos, dir, shell));
        }
    }
    let by_uid = || entries.iter().find(|entry| entry.1 == uid);
    let name = ["LOGNAME", "USER", "LNAME", "USERNAME"]
        .iter()
        .find_map(|key| host.env.get(*key).filter(|value| !value.is_empty()))
        .map(String::as_str)
        .or_else(|| by_uid().map(|entry| entry.0))
        .ok_or("the user has no entry in /etc/passwd")?;
    let entry = entries
        .iter()
        .find(|entry| entry.0 == name)
        .ok_or_else(|| format!("{name} has no entry in /etc/passwd"))?;
    let mut facts = Map::new();
    facts.insert("user_id".into(), Value::from(name));
    facts.insert("user_uid".into(), Value::from(entry.1));
    facts.insert("user_gid".into(), Value::from(entry.2));
    facts.insert("user_gecos".into(), Value::from(entry.3));
    facts.insert("user_dir".into(), Value::from(entry.4));
    facts.insert("user_shell".into(), Value::from(entry.5));
    facts.insert("real_user_id".into(), Value::from(uid));
    facts.insert("effective_user_id".into(), Value::from(euid));
    facts.insert("real_group_id".into(), Value::from(gid));
    // The reference reads the real group id twice.
    facts.insert("effective_group_id".into(), Value::from(gid));
    Ok(facts)
}

#[cfg(test)]
mod tests;
