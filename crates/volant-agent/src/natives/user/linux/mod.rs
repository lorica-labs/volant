// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::NativeRun;
use crate::modules::Context;
use crate::natives::common::{access, clock, invocation, lookup_user, native_run};
use crate::natives::group::{
    NSSWITCH, bin, entry, failure, host_gate, outcome, resolve_group, run_command,
};
use crate::natives::runtime::{Clock, Stop, py_strip};
use home::create_home;
#[cfg(all(test, target_os = "linux"))]
use home::setting;
#[cfg(all(test, target_os = "linux"))]
use request::hostname;
use request::{Request, SPEC, request};

mod home;
mod request;

pub fn run(
    args: &Map<String, Value>,
    context: &Context,
    cancelled: &dyn Fn() -> bool,
) -> NativeRun {
    native_run(
        answer(args, context, clock(context, cancelled), NSSWITCH),
        context,
    )
}

struct Account {
    uid: u32,
    gid: u32,
    comment: String,
    home: String,
    shell: String,
}

fn passwd_entry(name: &str) -> Result<Option<Account>, String> {
    Ok(entry("/etc/passwd", 7, name)?.and_then(|fields| {
        Some(Account {
            uid: fields[2].parse().ok()?,
            gid: fields[3].parse().ok()?,
            comment: fields[4].clone(),
            home: fields[5].clone(),
            shell: fields[6].clone(),
        })
    }))
}

/// The account named `name` as the module finds it through `pwd`, from `/etc/passwd`: `None`
/// when the host has no such account. An account only the name service knows hands back:
/// the commands would change the files, not the directory.
fn find_user(name: &str, clock: Clock) -> Result<Option<Account>, Stop> {
    let known = lookup_user(name, clock)?;
    let local = passwd_entry(name)?;
    match (known, local) {
        (None, None) => Ok(None),
        (Some(known), Some(local)) if known.uid == local.uid => Ok(Some(local)),
        (Some(_), None) => {
            Err("the name service knows this account and /etc/passwd does not".into())
        }
        _ => Err("/etc/passwd and the name service disagree about this account".into()),
    }
}

/// `os.path.dirname`.
fn dirname(path: &str) -> &str {
    let head = &path[..path.rfind('/').map_or(0, |at| at + 1)];
    if head.bytes().any(|b| b != b'/') {
        head.trim_end_matches('/')
    } else {
        head
    }
}

/// `set(x.strip() for x in groups.split(',') if x)`, in the order given. The module's set
/// has no order of its own: Python's string hashing changes it from run to run.
fn listed(groups: &str) -> Vec<&str> {
    let mut names = Vec::new();
    for name in groups.split(',').filter(|x| !x.is_empty()).map(py_strip) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn no_group() -> Stop {
    Stop::HandBack("a group given does not exist, which the module fails on".into())
}

/// `useradd`'s arguments, `create_user_useradd` for the arguments the native takes.
fn create(r: &Request, context: &Context, clock: Clock) -> Result<(PathBuf, Vec<String>), Stop> {
    if let Some(home) = r.home
        && r.create_home
        && !Path::new(dirname(home)).is_dir()
    {
        return Err("the module creates the home's missing parent itself".into());
    }
    let useradd = bin(context, "useradd")?;
    let mut argv = Vec::new();
    if let Some(uid) = r.uid {
        argv.extend(["-u".to_string(), uid.to_string()]);
    }
    if let Some(group) = r.group {
        resolve_group(group, clock)?.ok_or_else(no_group)?;
        argv.extend(["-g".to_string(), group.to_string()]);
    } else if resolve_group(r.name, clock)?.is_some() {
        // A group of the account's name exists: no user group.
        argv.push("-N".into());
    }
    if let Some(groups) = r.groups.as_deref().filter(|groups| !groups.is_empty()) {
        let names = listed(groups);
        for name in &names {
            resolve_group(name, clock)?.ok_or_else(no_group)?;
        }
        argv.extend(["-G".to_string(), names.join(",")]);
    }
    if let Some(comment) = r.comment {
        argv.extend(["-c".to_string(), comment.to_string()]);
    }
    if let Some(home) = r.home {
        argv.extend(["-d".to_string(), home.to_string()]);
    }
    if let Some(shell) = r.shell {
        argv.extend(["-s".to_string(), shell.to_string()]);
    }
    argv.push(if r.create_home { "-m" } else { "-M" }.into());
    if r.system {
        argv.push("-r".into());
    }
    argv.push(r.name.to_string());
    Ok((useradd, argv))
}

/// The groups whose members name the account, over the whole name service as `grp.getgrall()`
/// lists them.
fn memberships(context: &Context, clock: Clock, name: &str) -> Result<Vec<String>, Stop> {
    let (rc, out, _) = run_command(context, clock, &bin(context, "getent")?, &["group"])?;
    if rc != 0 {
        return Err(Stop::HandBack(format!("getent group exited {rc}")));
    }
    Ok(out
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(':').collect();
            (parts.len() >= 4 && parts[3].split(',').any(|member| member == name))
                .then(|| parts[0].to_string())
        })
        .collect())
}

/// `_check_usermod_append`: whether this `usermod` lists `-a, --append` in its help.
fn has_append(context: &Context, clock: Clock, usermod: &Path) -> Result<bool, Stop> {
    if !usermod
        .to_str()
        .is_some_and(|path| access(path, libc::X_OK))
    {
        return Ok(false);
    }
    let (_, out, err) = run_command(context, clock, usermod, &["--help"])?;
    Ok(format!("{out}{err}")
        .split('\n')
        .any(|line| py_strip(line).starts_with("-a, --append")))
}

/// `usermod`'s arguments, `modify_user_usermod` for the arguments the native takes, or `None`
/// when nothing differs.
fn modify(
    r: &Request,
    account: &Account,
    context: &Context,
    clock: Clock,
) -> Result<Option<(PathBuf, Vec<String>)>, Stop> {
    // The module creates a missing home after `usermod`, under `create_home`.
    if r.create_home && !Path::new(r.home.unwrap_or(&account.home)).exists() {
        return Err("the module creates the missing home itself".into());
    }
    let usermod = bin(context, "usermod")?;
    let mut argv = Vec::new();
    if let Some(uid) = r.uid.filter(|uid| *uid != account.uid) {
        argv.extend(["-u".to_string(), uid.to_string()]);
    }
    if let Some(group) = r.group {
        let group = resolve_group(group, clock)?.ok_or_else(no_group)?;
        if group.gid != account.gid {
            argv.extend(["-g".to_string(), group.gid.to_string()]);
        }
    }
    if let Some(groups) = &r.groups {
        let current = memberships(context, clock, r.name)?;
        let mut wanted: Vec<String> = Vec::new();
        for name in listed(groups) {
            let group = resolve_group(name, clock)?.ok_or_else(no_group)?;
            if !wanted.contains(&group.name) {
                wanted.push(group.name);
            }
        }
        let added = wanted.iter().any(|group| !current.contains(group));
        let removed = current.iter().any(|group| !wanted.contains(group));
        let change = if r.append {
            if added && !has_append(context, clock, &usermod)? {
                return Err("this usermod has no --append: the module passes -A".into());
            }
            if added {
                argv.push("-a".into());
            }
            added
        } else {
            added || removed
        };
        if change {
            argv.extend(["-G".to_string(), wanted.join(",")]);
        }
    }
    if let Some(comment) = r.comment.filter(|comment| *comment != account.comment) {
        argv.extend(["-c".to_string(), comment.to_string()]);
    }
    if let Some(home) = r.home.filter(|home| *home != account.home) {
        argv.extend(["-d".to_string(), home.to_string()]);
    }
    if let Some(shell) = r.shell.filter(|shell| *shell != account.shell) {
        argv.extend(["-s".to_string(), shell.to_string()]);
    }
    if argv.is_empty() {
        return Ok(None);
    }
    argv.push(r.name.to_string());
    Ok(Some((usermod, argv)))
}

fn answer(
    args: &Map<String, Value>,
    context: &Context,
    clock: Clock,
    nsswitch: &str,
) -> Result<Map<String, Value>, Stop> {
    let mut invocation = invocation(SPEC, args);
    // `type='list'` splits a string on commas, and `invocation` shows the list.
    if let Some(Value::String(groups)) = invocation["module_args"].get("groups").cloned() {
        invocation["module_args"]["groups"] = Value::from(groups.split(',').collect::<Vec<_>>());
    }
    let params = invocation["module_args"].as_object().unwrap();
    let r = request(args, params)?;
    host_gate(nsswitch)?;
    for file in ["/etc/redhat-release", "/etc/SuSE-release"] {
        if Path::new(file).exists() {
            return Err(format!("{file} changes the module's useradd arguments").into());
        }
    }
    let found = find_user(r.name, clock)?;
    // Decided, and every command found, before anything runs.
    let command = match (r.present, &found) {
        (false, None) => None,
        (false, Some(_)) => Some((bin(context, "userdel")?, vec![r.name.to_string()])),
        (true, None) => Some(create(&r, context, clock)?),
        (true, Some(account)) => modify(&r, account, context, clock)?,
    };
    let ran = match command {
        Some((program, argv)) => {
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            let ran = run_command(context, clock, &program, &argv)?;
            if ran.0 != 0 {
                return Ok(failure(
                    invocation.clone(),
                    &[
                        ("name", Value::from(r.name)),
                        ("msg", Value::from(ran.2)),
                        ("rc", Value::from(ran.0)),
                    ],
                ));
            }
            Some(ran)
        }
        None => None,
    };
    let mut result = Map::new();
    result.insert("name".into(), Value::from(r.name));
    result.insert("state".into(), params["state"].clone());
    match (r.present, found.is_some()) {
        (false, true) => {
            result.insert("force".into(), params["force"].clone());
            result.insert("remove".into(), params["remove"].clone());
        }
        (true, false) => {
            result.insert("system".into(), Value::Bool(r.system));
            result.insert("create_home".into(), Value::Bool(r.create_home));
        }
        (true, true) => {
            result.insert("append".into(), Value::Bool(r.append));
            result.insert("move_home".into(), params["move_home"].clone());
        }
        (false, false) => {}
    }
    outcome(&mut result, ran.as_ref());
    if r.present {
        match passwd_entry(r.name) {
            Ok(Some(account)) => {
                let home = r.home.unwrap_or(&account.home).to_string();
                result.insert("uid".into(), Value::from(account.uid));
                result.insert("group".into(), Value::from(account.gid));
                result.insert("comment".into(), Value::from(account.comment));
                result.insert("home".into(), Value::from(account.home));
                result.insert("shell".into(), Value::from(account.shell));
                if let Some(groups) = &r.groups {
                    result.insert("groups".into(), Value::from(groups.as_str()));
                }
                // Ruled out before a `usermod`, which hands back; after `useradd -m`, which
                // creates the home or fails, the module's own creation.
                if r.create_home && !Path::new(&home).exists() {
                    let made = create_home(
                        Path::new("/etc/skel"),
                        Path::new("/etc/login.defs"),
                        Path::new(&home),
                        account.uid,
                        account.gid,
                    );
                    if let Err(msg) = made {
                        return Ok(failure(invocation.clone(), &[("msg", Value::from(msg))]));
                    }
                    result.insert("changed".into(), Value::Bool(true));
                }
            }
            Ok(None) => {}
            Err(reason) => {
                return Ok(failure(invocation.clone(), &[("msg", Value::from(reason))]));
            }
        }
    }
    result.insert("invocation".into(), invocation.clone());
    Ok(result)
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
