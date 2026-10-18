// SPDX-License-Identifier: GPL-3.0-or-later
//! Running the collectors a request names, and filtering their facts the way the reference does.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use serde_json::{Map, Value};

use super::fnmatch::fnmatch;
use super::{
    Host, LsbRelease, Request, apparmor, caps, cmdline, date_time, distribution, dns, env, fips,
    hardware, local, lsb, network, pkg_mgr, platform, python, selinux, service_mgr, ssh_pub_keys,
    user,
};
use crate::modules::Context;
use crate::natives::runtime::{Clock, Root, Stop, wait_polling_cancel};

/// The facts, under the names the module returns them, or the reason to hand back.
///
/// The distribution's `ID` first: it decides whether the host is inside the subset at all for
/// one file read, where the interpreter and `lsb_release` cost tens of milliseconds.
#[expect(clippy::too_many_lines)]
pub(super) fn collect(
    request: &Request,
    root: &Root,
    context: &Context,
    agent: Option<BTreeMap<String, String>>,
    clock: Clock,
) -> Result<Map<String, Value>, Stop> {
    for key in context.environment.keys() {
        if key == "LANG" || key == "LANGUAGE" || key == "TZ" || key.starts_with("LC_") {
            return Err(format!(
                "the task sets {key}, which changes what the module reads of its locale"
            )
            .into());
        }
    }
    distribution::gate(root)?;
    let interpreter = context
        .interpreter
        .as_deref()
        .ok_or("no interpreter to read the python facts from")?;
    // `lsb_release` and `ip` need the module's environment, not the interpreter: they run beside
    // the probe in the environment the module has when its interpreter adds nothing to the
    // agent's, and again after the probe when it did (Python sets `LC_CTYPE` in a C locale).
    let guess: Option<BTreeMap<String, String>> = agent.map(|mut env| {
        env.extend(context.environment.clone());
        env
    });
    // The early runs cannot ask the task's cancel, which answers once and on this thread. They
    // stop at the deadline, or when this thread raises `halt`: as soon as the probe ends
    // without an answer, and when the cancel arrives while this thread waits for them.
    let halt = AtomicBool::new(false);
    let halted = || halt.load(Ordering::Relaxed);
    let halted: &(dyn Fn() -> bool + Sync) = &halted;
    let deadline = clock.deadline;
    let (probe, early, early_network) = std::thread::scope(|scope| {
        let early_clock = move || Clock {
            deadline,
            cancelled: halted,
        };
        let early = guess.as_ref().map(|env| {
            let (sent, answer) = mpsc::channel();
            scope.spawn(move || {
                let _ = sent.send(LsbRelease::run(root, env, early_clock()));
            });
            answer
        });
        let network = guess.as_ref().filter(|_| request.network).map(|env| {
            let (sent, answer) = mpsc::channel();
            scope.spawn(move || {
                let _ = sent.send(network::collect(root, env, early_clock()));
            });
            answer
        });
        let probe = python::probe(interpreter, &context.environment, clock);
        if probe.is_err() {
            halt.store(true, Ordering::Relaxed);
        }
        let early = match &early {
            Some(answer) => wait_polling_cancel(answer, clock, &halt)?,
            None => None,
        };
        let early_network = match &network {
            Some(answer) => wait_polling_cancel(answer, clock, &halt)?,
            None => None,
        };
        Ok::<_, Stop>((probe, early, early_network))
    })?;
    let probe = probe?;
    let mut env = probe.env.clone();
    env.extend(context.environment.clone());
    let same_env = guess.as_ref() == Some(&env);
    let lsb_release = match early {
        Some(early) if same_env => early?,
        _ => LsbRelease::run(root, &env, clock)?,
    };
    let network = match early_network {
        Some(early) if same_env => early?,
        _ if request.network => network::collect(root, &env, clock)?,
        _ => Map::new(),
    };
    let host = Host {
        root,
        env,
        probe: &probe,
        clock,
    };
    let collected = [
        distribution::collect(&host, &lsb_release)?,
        apparmor::collect(&host),
        caps::collect(&host)?,
        cmdline::collect(&host),
        date_time::collect(&host)?,
        dns::collect(&host),
        env::collect(&host),
        fips::collect(&host),
        local::collect(&host, request.fact_path.as_deref())?,
        lsb::collect(&host, &lsb_release)?,
        pkg_mgr::collect(&host)?,
        platform::collect(&host)?,
        python::collect(&host),
        selinux::collect(&host)?,
        service_mgr::collect(&host)?,
        ssh_pub_keys::collect(&host)?,
        user::collect(&host)?,
    ];
    let prefixed = |collected: Map<String, Value>| -> Map<String, Value> {
        collected
            .into_iter()
            .map(|(name, value)| (format!("ansible_{}", name.replace('-', "_")), value))
            .collect()
    };
    let mut groups: Vec<Map<String, Value>> = collected.into_iter().map(prefixed).collect();
    groups.push(prefixed(network));
    if request.hardware {
        // The reference counts processors by the architecture the platform collector found,
        // whatever the filter keeps.
        let architecture = groups
            .iter()
            .find_map(|group| group.get("ansible_architecture"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        groups.push(prefixed(hardware::collect(root, &architecture)));
    }
    let mut meta = Map::new();
    meta.insert(
        "gather_subset".into(),
        Value::from(request.gather_subset.clone()),
    );
    meta.insert("module_setup".into(), Value::Bool(true));
    groups.push(meta);
    let mut facts = Map::new();
    for group in groups {
        filtered(group, &request.filter, &mut facts);
    }
    Ok(facts)
}

/// The facts of one collector that `filter` keeps, added to `facts` in the reference's order:
/// `AnsibleFactCollector._filter` in ansible-core 2.19.12 walks the patterns, and for each one
/// the collector's facts, and keeps a fact whose name matches the pattern (fnmatch), or matches
/// it with `ansible_` in front when the pattern starts with none of `ansible_`, `facter` and
/// `ohai`. An empty pattern keeps everything, and so does an empty filter. The collectors'
/// metadata (`gather_subset`, `module_setup`) is filtered like any collector's facts.
pub(super) fn filtered(
    group: Map<String, Value>,
    filter: &[String],
    facts: &mut Map<String, Value>,
) {
    if filter.is_empty() {
        facts.extend(group);
        return;
    }
    for pattern in filter {
        let prefixed = (!pattern.starts_with("ansible_")
            && !pattern.starts_with("facter")
            && !pattern.starts_with("ohai"))
        .then(|| format!("ansible_{pattern}"));
        for (name, value) in &group {
            if pattern.is_empty()
                || fnmatch(name, pattern)
                || prefixed.as_deref().is_some_and(|p| fnmatch(name, p))
            {
                facts.insert(name.clone(), value.clone());
            }
        }
    }
}
