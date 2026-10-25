// SPDX-License-Identifier: GPL-3.0-or-later
//! What the module runs when its cache is stale or a package is missing, in its order and under
//! its environment: `apt-get update` (python-apt's `cache.update()` there, which runs the same
//! hooks and touches the same stamp), the candidates of the missing names, `apt-get install` built
//! as `install()` builds it (under `policy_rc_d` when the task sets it), then `apt-mark manual`
//! over every name.
//!
//! Handed back before anything changed: a missing name `apt-cache policy` gives no candidate
//! (unknown, virtual, or pinned away), whose failure the module words itself; a tool not on
//! `PATH`. Handed back after the change it attempted: an `apt-get update` that failed, which the
//! module retries with its backoff, warnings and message; a name that lost its candidate in the
//! update.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::super::runtime::{self, Clock, Root, Stop};
use super::status::{bin_path, cache_mtime, from_timestamp};
use super::{cache, policy};

/// What the plan leaves to run.
pub(super) struct Act {
    pub(super) update: bool,
    /// Every name the task gives, for `apt-mark manual`; none for a cache update alone.
    pub(super) names: Vec<String>,
    /// The names not installed, in the task's order, for `apt-get install`.
    pub(super) missing: Vec<String>,
    /// dpkg's native architecture, as `apt-cache policy` may name a package with it.
    pub(super) arch: String,
    /// The exit code `/usr/sbin/policy-rc.d` gives during the install, when the task sets one.
    pub(super) policy_rc_d: Option<i64>,
    /// The module's arguments, defaults filled in.
    pub(super) params: Map<String, Value>,
    /// `cache_update_time` before any update.
    pub(super) cache_time: i64,
    /// The result so far: `invocation`.
    pub(super) result: Map<String, Value>,
}

/// `get_best_parsable_locale`'s preferences, the first `locale -a` lists winning.
const LOCALES: [&str; 6] = [
    "C.utf8",
    "C.UTF-8",
    "en_US.utf8",
    "en_US.UTF-8",
    "C",
    "POSIX",
];

pub(super) fn act(
    act: Act,
    root: &Root,
    task_env: &BTreeMap<String, String>,
    clock: Clock,
) -> Result<Map<String, Value>, Stop> {
    let apt_get = bin_path(task_env, "apt-get").ok_or("apt-get is not on PATH")?;
    // The module splits `"<apt-get> -y ..."` with `shlex`: the path must read back as itself.
    let apt_get = apt_get
        .to_str()
        .filter(|path| shlex::split(path).is_some_and(|words| words == [*path]))
        .ok_or("the module reads this apt-get path differently")?
        .to_string();
    let env = apt_env(task_env, clock)?;
    let private = cache::options(&env, task_env, clock)?;
    // Asked before anything changes; asked again only after an update, which can move them.
    let mut pins = if act.missing.is_empty() {
        None
    } else {
        Some(candidates(
            &act.missing,
            &act.arch,
            &private,
            &env,
            task_env,
            clock,
        )?)
    };
    let mut cache_updated = false;
    let mut cache_time = act.cache_time;
    if act.update {
        let mut args: Vec<&str> = private.iter().map(String::as_str).collect();
        args.push("update");
        let (rc, _, _) = output(&env, clock, Path::new(&apt_get), &args)?;
        if rc != 0 {
            return Err("apt-get update failed, which the module retries".into());
        }
        let after = match cache_mtime(root)? {
            Some((sec, nsec)) => from_timestamp(sec, nsec).0,
            None => 0,
        };
        cache_updated = after != cache_time;
        cache_time = after;
    }
    let mut result = act.result;
    let cache = json_map(json!({"cache_updated": cache_updated, "cache_update_time": cache_time}));
    if act.names.is_empty() {
        result.insert("changed".into(), cache_updated.into());
        result.extend(cache);
        return Ok(result);
    }
    let mut outcome = Map::new();
    if act.missing.is_empty() {
        outcome.insert("changed".into(), false.into());
    } else {
        let pins = match pins.take() {
            Some(pins) if !act.update => pins,
            _ => candidates(&act.missing, &act.arch, &private, &env, task_env, clock)?,
        };
        let cmd = install_command(&apt_get, &act.params, &pins);
        let policy = match act.policy_rc_d {
            Some(code) => match policy::enter(code, root, task_env)? {
                Ok(policy) => Some(policy),
                Err(msg) => return Ok(failed(result, msg)),
            },
            None => None,
        };
        let ran = run_line(&cmd, &private, &env, clock);
        if let Some(policy) = policy
            && let Err(msg) = policy::exit(policy, root)
        {
            return Ok(failed(result, msg));
        }
        let (rc, out, err) = ran?;
        if rc == 0 {
            outcome.extend(json_map(json!({
                "changed": true, "stdout": out, "stderr": err, "diff": {},
            })));
        } else {
            outcome.extend(failure(&cmd, out, err, rc));
        }
    }
    // `mark_installed_manually`, whatever the install did; its failure is the task's.
    let apt_mark = bin_path(task_env, "apt-mark").ok_or("apt-mark is not on PATH")?;
    let mut cmd = format!("{} manual {}", apt_mark.display(), act.names.join(" "));
    let (mut rc, mut out, mut err) = run_line(&cmd, &private, &env, clock)?;
    if err.contains("Invalid operation")
        || err.contains("Usage: apt-mark [options] {markauto|unmarkauto} packages")
    {
        cmd = format!("{} unmarkauto {}", apt_mark.display(), act.names.join(" "));
        (rc, out, err) = run_line(&cmd, &private, &env, clock)?;
    }
    if rc != 0 {
        result.extend(failure(&cmd, out, err, rc));
        return Ok(result);
    }
    result.extend(outcome);
    result.extend(cache);
    Ok(result)
}

/// The task's environment with the module's `APT_ENV_VARS` over it.
fn apt_env(
    task_env: &BTreeMap<String, String>,
    clock: Clock,
) -> Result<BTreeMap<String, String>, Stop> {
    let mut locale = "C".to_string();
    if let Some(program) = bin_path(task_env, "locale")
        && let Ok((0, out, _)) = runtime::run_output(task_env, clock, &program, &["-a"])?
    {
        let available: Vec<&str> = runtime::splitlines(runtime::py_strip(&out));
        if let Some(found) = LOCALES.iter().find(|l| available.contains(l)) {
            locale = (*found).to_string();
        }
    }
    let mut env = task_env.clone();
    env.insert("DEBIAN_FRONTEND".into(), "noninteractive".into());
    env.insert("DEBIAN_PRIORITY".into(), "critical".into());
    for name in ["LANG", "LC_ALL", "LC_MESSAGES", "LC_CTYPE", "LANGUAGE"] {
        env.insert(name.into(), locale.clone());
    }
    Ok(env)
}

/// `name=<candidate>` for each missing name: the version `apt-cache policy` gives as candidate,
/// which is the one `package_best_match` takes, pins and all.
fn candidates(
    missing: &[String],
    arch: &str,
    cache: &[String],
    env: &BTreeMap<String, String>,
    task_env: &BTreeMap<String, String>,
    clock: Clock,
) -> Result<Vec<String>, Stop> {
    let apt_cache = bin_path(task_env, "apt-cache").ok_or("apt-cache is not on PATH")?;
    let mut args: Vec<&str> = cache.iter().map(String::as_str).collect();
    args.push("policy");
    args.extend(missing.iter().map(String::as_str));
    let (rc, out, _) = output(env, clock, &apt_cache, &args)?;
    if rc != 0 {
        return Err("apt-cache policy failed".into());
    }
    let mut found = BTreeMap::new();
    let mut current = None;
    for line in out.lines() {
        if !line.starts_with(' ') {
            current = line.strip_suffix(':');
        } else if let (Some(name), Some(candidate)) =
            (current, line.trim().strip_prefix("Candidate: "))
        {
            found.insert(name, candidate);
        }
    }
    missing
        .iter()
        .map(|name| {
            found
                .get(name.as_str())
                .or_else(|| found.get(format!("{name}:{arch}").as_str()))
                .filter(|version| **version != "(none)")
                .map(|version| format!("{name}={version}"))
                .ok_or_else(|| Stop::from("a missing package has no candidate the native can name"))
        })
        .collect()
}

/// `install()`'s command line, spaces of its empty options included, as its failure quotes it.
fn install_command(apt_get: &str, params: &Map<String, Value>, pins: &[String]) -> String {
    let on = |key: &str| params.get(key) == Some(&Value::Bool(true));
    let mut expanded = String::new();
    for option in params
        .get("dpkg_options")
        .and_then(Value::as_str)
        .unwrap_or("force-confdef,force-confold")
        .split(',')
    {
        let _ = write!(expanded, " -o \"Dpkg::Options::=--{option}\"");
    }
    let dpkg_options = format!(
        "{} -o DPkg::Lock::Timeout={}",
        expanded.trim(),
        params
            .get("lock_timeout")
            .and_then(Value::as_i64)
            .unwrap_or(60)
    );
    let force_yes = if on("force") { "--force-yes" } else { "" };
    let no_remove = if on("fail_on_autoremove") {
        "--no-remove"
    } else {
        ""
    };
    let packages: Vec<String> = pins.iter().map(|pin| format!("'{pin}'")).collect();
    // The empty ones: `only_upgrade`, `fixed`, `autoremove` and the check-mode `--simulate`.
    let mut cmd = format!(
        "{apt_get} -y {dpkg_options} {} {} {force_yes} {} {no_remove} {} install {}",
        "",
        "",
        "",
        "",
        packages.join(" ")
    );
    match params.get("install_recommends") {
        Some(Value::Bool(false)) => cmd.push_str(" -o APT::Install-Recommends=no"),
        Some(Value::Bool(true)) => cmd.push_str(" -o APT::Install-Recommends=yes"),
        _ => {}
    }
    for (key, flag) in [
        ("allow_unauthenticated", " --allow-unauthenticated"),
        ("allow_downgrade", " --allow-downgrades"),
        (
            "allow_change_held_packages",
            " --allow-change-held-packages",
        ),
    ] {
        if on(key) {
            cmd.push_str(flag);
        }
    }
    cmd
}

/// `run_command(cmd)` of a command line, which the module splits with `shlex`, with `cache`
/// after the program: the line stays the one the module's messages quote.
fn run_line(
    cmd: &str,
    cache: &[String],
    env: &BTreeMap<String, String>,
    clock: Clock,
) -> Result<(i32, String, String), Stop> {
    let argv = shlex::split(cmd).ok_or("the module would split this command line otherwise")?;
    let (program, args) = argv.split_first().ok_or("an empty command line")?;
    let args: Vec<&str> = cache.iter().chain(args).map(String::as_str).collect();
    output(env, clock, Path::new(program), &args)
}

fn output(
    env: &BTreeMap<String, String>,
    clock: Clock,
    program: &Path,
    args: &[&str],
) -> Result<(i32, String, String), Stop> {
    runtime::run_output(env, clock, program, args)?
        .map_err(|_| Stop::HandBack(format!("{} could not be started", program.display())))
}

/// `fail_json(msg="'<cmd>' failed: <stderr>", stdout, stderr, rc)`.
fn failure(cmd: &str, out: String, err: String, rc: i32) -> Map<String, Value> {
    json_map(json!({
        "failed": true,
        "msg": format!("'{cmd}' failed: {err}"),
        "stdout": out,
        "stderr": err,
        "rc": rc,
    }))
}

/// `fail_json(msg)` alone.
fn failed(mut result: Map<String, Value>, msg: String) -> Map<String, Value> {
    result.insert("failed".into(), true.into());
    result.insert("msg".into(), msg.into());
    result
}

fn json_map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => unreachable!("a JSON object"),
    }
}
