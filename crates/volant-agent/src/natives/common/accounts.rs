// SPDX-License-Identifier: GPL-3.0-or-later
//! Users and groups as the reference looks them up, and the owner and group arguments resolved
//! to ids.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::natives::runtime::{Clock, Stop, run as run_program};

/// An account of `/etc/passwd`, or of the host's name service.
pub struct Passwd {
    pub name: String,
    pub uid: u32,
}

/// A group of `/etc/group`, or of the host's name service.
pub struct Group {
    pub name: String,
    pub gid: u32,
}

/// The account `name_or_uid` names, by uid when it is all digits, as `pwd.getpwnam` or
/// `pwd.getpwuid` would find it: `Ok(None)` when the host has no such account, `Err` when that
/// cannot be told.
///
/// The agent is a static musl binary, with no NSS. `/etc/passwd` answers when `nsswitch.conf`
/// puts `files` first; an account it does not hold is asked of `getent`, which runs through the
/// host's own C library and so sees LDAP, sssd or systemd's dynamic users as Python does.
///
/// `getent` runs through `command`'s executor under `clock`: the task's `timeout` and cancel
/// reach a name service that hangs, as they reach the Python module's own lookup.
pub fn lookup_user(name_or_uid: &str, clock: Clock) -> Result<Option<Passwd>, Stop> {
    Ok(lookup("passwd", "/etc/passwd", 7, name_or_uid, clock)?
        .map(|(name, uid)| Passwd { name, uid }))
}

/// The group `name_or_gid` names, as `lookup_user` finds an account.
pub fn lookup_group(name_or_gid: &str, clock: Clock) -> Result<Option<Group>, Stop> {
    Ok(
        lookup("group", "/etc/group", 4, name_or_gid, clock)?
            .map(|(name, gid)| Group { name, gid }),
    )
}

/// The name and id of the entry `key` names in the database `db`, read from `file` then from
/// `getent`. An entry is `fields` colon-separated fields with the id third.
fn lookup(
    db: &str,
    file: &str,
    fields: usize,
    key: &str,
    clock: Clock,
) -> Result<Option<(String, u32)>, Stop> {
    let id = if !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()) {
        Some(
            key.parse::<u32>()
                .map_err(|_| format!("the {db} id is out of range"))?,
        )
    } else {
        None
    };
    let entry = |line: &str| -> Option<(String, u32)> {
        let parts: Vec<&str> = line.split(':').collect();
        let entry_id = parts.get(2)?.parse::<u32>().ok()?;
        let hit = match id {
            Some(id) => entry_id == id,
            None => parts[0] == key,
        };
        (parts.len() >= fields && hit).then(|| (parts[0].to_string(), entry_id))
    };
    if files_first("/etc/nsswitch.conf", db) {
        let text = fs::read_to_string(file).map_err(|err| format!("reading {file}: {err}"))?;
        for line in text.lines() {
            if line.starts_with(['+', '-']) {
                return Err(format!("{file} has NIS compat entries").into());
            }
            if let Some(found) = entry(line) {
                return Ok(Some(found));
            }
        }
    }
    let no_env = BTreeMap::new();
    match run_program(&no_env, clock, Path::new("getent"), &[db, key])? {
        Some((0, out)) => Ok(Some(out.lines().find_map(entry).ok_or_else(|| {
            Stop::HandBack(format!("getent {db} answered no entry for the key"))
        })?)),
        Some((2, _)) => Ok(None),
        Some((rc, _)) => Err(Stop::HandBack(format!("getent {db} exited {rc}"))),
        None => Err(Stop::HandBack("getent cannot be run".into())),
    }
}

/// Whether `nsswitch`, the host's `/etc/nsswitch.conf`, asks `files` first for `db`, so that an
/// entry found there is the one the C library returns. glibc's default, without the file, is
/// `files` first as well.
pub(crate) fn files_first(nsswitch: &str, db: &str) -> bool {
    let Ok(conf) = fs::read_to_string(nsswitch) else {
        return true;
    };
    conf.lines()
        .filter_map(|line| line.split('#').next()?.trim().strip_prefix(db))
        .find_map(|rest| rest.trim_start().strip_prefix(':'))
        .is_none_or(|sources| matches!(sources.split_whitespace().next(), Some("files" | "compat")))
}

/// An `owner` or `group` argument, resolved before anything changes.
pub enum Account {
    Id(u32),
    /// No such name on the host: the reference fails when it reaches this attribute.
    Unknown(String),
}

/// `owner` as `set_owner_if_different` reads it: a number is a uid, anything else a name.
pub fn owner_account(owner: &str, clock: Clock) -> Result<Account, Stop> {
    account(owner, |name| Ok(lookup_user(name, clock)?.map(|p| p.uid)))
}

/// `group` as `set_group_if_different` reads it.
pub fn group_account(group: &str, clock: Clock) -> Result<Account, Stop> {
    account(group, |name| Ok(lookup_group(name, clock)?.map(|g| g.gid)))
}

fn account(given: &str, find: impl Fn(&str) -> Result<Option<u32>, Stop>) -> Result<Account, Stop> {
    if !given.is_empty() && given.bytes().all(|b| b.is_ascii_digit()) {
        return given
            .parse()
            .map(Account::Id)
            .map_err(|_| Stop::HandBack("the id is out of range".into()));
    }
    // Python's `int()` also takes a sign, underscores, spaces and other scripts' digits.
    if given.is_empty()
        || (given.chars().any(char::is_numeric)
            && given
                .chars()
                .all(|c| c.is_numeric() || c.is_whitespace() || "+-_".contains(c)))
    {
        return Err(Stop::HandBack(
            "the account is read as a number or not at all by Python".into(),
        ));
    }
    Ok(find(given)?.map_or_else(|| Account::Unknown(given.to_string()), Account::Id))
}
