// SPDX-License-Identifier: GPL-3.0-or-later
//! The `user` collector: `getpass.getuser()`, which reads the environment before the password
//! database, and that user's entry. The entry is read from `/etc/passwd` when the name service
//! looks there first; a user only another service knows hands back.

use serde_json::{Map, Value};

use super::{Host, py_split};

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
mod tests {
    use serde_json::json;

    use super::super::tests::{FakeRoot, probe};
    use super::*;

    /// The name comes from `LOGNAME` before the uid, as `getpass.getuser()` has it, and its entry
    /// from `/etc/passwd`.
    ///
    /// What would make this red: the name taken from the uid while `LOGNAME` says otherwise
    /// (under `sudo -u` without `-i` they differ), or the effective group read from `getegid`,
    /// which the reference does not do.
    #[test]
    fn the_user_is_the_environment_s_then_the_uid_s() {
        let fake = FakeRoot::new("user");
        fake.write(
            "/etc/nsswitch.conf",
            "# comment\npasswd:         files systemd\n",
        )
        .write(
            "/etc/passwd",
            "root:x:0:0:root:/root:/bin/bash\nuser:x:1000:1001:A User,,,:/home/user:/bin/sh\n",
        );
        let root = fake.root();
        let mut probe = probe();
        let facts = collect_as(&fake.host(&root, &probe), 0, 0, 5).unwrap();
        assert_eq!(
            Value::Object(facts),
            json!({"user_id": "user", "user_uid": 1000, "user_gid": 1001, "user_gecos": "A User,,,",
                   "user_dir": "/home/user", "user_shell": "/bin/sh", "real_user_id": 0,
                   "effective_user_id": 0, "real_group_id": 5, "effective_group_id": 5})
        );
        probe.env.remove("LOGNAME");
        let facts = collect_as(&fake.host(&root, &probe), 0, 0, 0).unwrap();
        assert_eq!(facts["user_id"], "root", "no variable, the uid's name");
        probe.env.insert("USER".into(), "nobody-here".into());
        assert!(
            collect_as(&fake.host(&root, &probe), 0, 0, 0).is_err(),
            "a user /etc/passwd lacks"
        );
        fake.write("/etc/nsswitch.conf", "passwd: sss files\n");
        probe.env.insert("USER".into(), "root".into());
        assert!(
            collect_as(&fake.host(&root, &probe), 0, 0, 0).is_err(),
            "sssd first"
        );
    }
}
