// SPDX-License-Identifier: GPL-3.0-or-later
//! The `lsb` collector: `lsb_release -a`, or `/etc/lsb-release` when that gives nothing, each line
//! matched by substring in the reference's order.

use serde_json::{Map, Value};

use super::{Host, LsbRelease, py_strip, splitlines};

const STRIP_QUOTES: &[char] = &['\'', '"', '\\'];

pub fn collect(host: &Host, lsb_release: &LsbRelease) -> Result<Map<String, Value>, String> {
    let mut lsb: Vec<(&str, String)> = Vec::new();
    // A dictionary's assignment: a key keeps its place and takes the new value.
    fn set(lsb: &mut Vec<(&'static str, String)>, key: &'static str, value: &str) {
        match lsb.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => *v = value.to_string(),
            None => lsb.push((key, value.to_string())),
        }
    }
    if let Some((0, out)) = &lsb_release.collector {
        for line in splitlines(out) {
            let Some((_, value)) = line.split_once(':') else {
                continue;
            };
            let value = py_strip(value);
            if line.contains("LSB Version:") {
                set(&mut lsb, "release", value);
            } else if line.contains("Distributor ID:") {
                set(&mut lsb, "id", value);
            } else if line.contains("Description:") {
                set(&mut lsb, "description", value);
            } else if line.contains("Release:") {
                set(&mut lsb, "release", value);
            } else if line.contains("Codename:") {
                set(&mut lsb, "codename", value);
            }
        }
    }
    if lsb.is_empty() && host.root.exists("/etc/lsb-release") {
        for line in host.root.lines("/etc/lsb-release") {
            // The reference indexes the part after `=`, and a line without one fails the whole
            // collector.
            let (_, value) = line
                .split_once('=')
                .ok_or("/etc/lsb-release has a line without '='")?;
            let value = py_strip(value);
            if line.contains("DISTRIB_ID") {
                set(&mut lsb, "id", value);
            } else if line.contains("DISTRIB_RELEASE") {
                set(&mut lsb, "release", value);
            } else if line.contains("DISTRIB_DESCRIPTION") {
                set(&mut lsb, "description", value);
            } else if line.contains("DISTRIB_CODENAME") {
                set(&mut lsb, "codename", value);
            }
        }
    }
    if let Some(release) = lsb
        .iter()
        .find(|(key, _)| *key == "release")
        .map(|(_, v)| v.clone())
    {
        let major = release.split('.').next().unwrap_or_default().to_string();
        lsb.push(("major_release", major));
    }
    let lsb = lsb
        .into_iter()
        .map(|(key, value)| {
            (
                key.to_string(),
                Value::from(value.trim_matches(STRIP_QUOTES)),
            )
        })
        .collect();
    let mut facts = Map::new();
    facts.insert("lsb".into(), Value::Object(lsb));
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::tests::{FakeRoot, probe};
    use super::*;

    fn lsb(fake: &FakeRoot) -> Result<Value, String> {
        let (root, probe) = (fake.root(), probe());
        let host = fake.host(&root, &probe);
        let run = LsbRelease::run(&host).unwrap();
        collect(&host, &run).map(|facts| facts["lsb"].clone())
    }

    /// `lsb_release -a` as Ubuntu 24.04 prints it, the file when the command is missing or
    /// fails, quotes stripped after `major_release` is cut.
    ///
    /// What would make this red: the file read when the command answered, the quotes of the
    /// file's description kept, or the command's failure taken as its answer.
    #[test]
    fn lsb_comes_from_the_command_then_the_file() {
        let fake = FakeRoot::new("lsb");
        assert_eq!(lsb(&fake).unwrap(), json!({}), "neither");
        fake.write(
            "/etc/lsb-release",
            "DISTRIB_ID=Ubuntu\nDISTRIB_RELEASE=24.04\nDISTRIB_CODENAME=noble\nDISTRIB_DESCRIPTION=\"Ubuntu 24.04.4 LTS\"\n",
        );
        let from_file = json!({"id": "Ubuntu", "release": "24.04", "codename": "noble",
                               "description": "Ubuntu 24.04.4 LTS", "major_release": "24"});
        assert_eq!(lsb(&fake).unwrap(), from_file);
        fake.command("/usr/bin/lsb_release", "Distributor ID:\tUbuntu", 1);
        assert_eq!(
            lsb(&fake).unwrap(),
            from_file,
            "a failing command reads the file"
        );
        fake.command(
            "/usr/bin/lsb_release",
            "Distributor ID:\tUbuntu\nDescription:\tUbuntu 24.04.4 LTS\nRelease:\t24.04\nCodename:\tnoble",
            0,
        );
        assert_eq!(
            lsb(&fake).unwrap(),
            json!({"id": "Ubuntu", "description": "Ubuntu 24.04.4 LTS", "release": "24.04",
                   "codename": "noble", "major_release": "24"})
        );
        fake.command("/usr/bin/lsb_release", "", 0)
            .write("/etc/lsb-release", "DISTRIB_ID=Ubuntu\nbroken\n");
        assert!(lsb(&fake).is_err());
    }
}
