// SPDX-License-Identifier: GPL-3.0-or-later
//! `setup`, answered in the agent on Debian and Ubuntu: the 17 collectors ansible-core 2.19.12
//! runs for `min`, each reading what the reference reads, in the reference's words, and the
//! processor, memory and address facts of its `hardware` and `network` collectors.
//!
//! A subset that also selects other collectors (`all`, the default) is answered without their
//! facts: a key the native does not produce is absent, never guessed, and the controller sends
//! `setup` here only for a play that reads none of them. A subset that names one of them hands
//! back, as does anything else the native cannot reproduce: another distribution, SELinux
//! enabled, a host name only DNS knows, a locale the module would replace. Handing back is always
//! safe here, because collecting changes nothing on the host.
//!
//! Every file is read under a [`Root`], the real `/` or a test directory, and every command is
//! found through the module's own `PATH` under the same root.

mod apparmor;
mod caps;
mod cmdline;
mod collect;
mod date_time;
mod distribution;
mod dns;
mod env;
mod fips;
mod fnmatch;
mod hardware;
mod local;
mod lsb;
mod network;
mod pkg_mgr;
mod platform;
mod python;
mod request;
mod selinux;
mod service_mgr;
mod ssh_pub_keys;
mod user;

use std::collections::BTreeMap;
use std::time::Instant;

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use crate::modules::Context;
use crate::natives::common::{ArgSpec, invocation};
use crate::natives::runtime::{Clock, Root, Stop, bin_path, exec_path, run, unbounded};
use crate::natives::{Native, NativeRun};

use collect::collect;
use request::{Request, request};

pub const NATIVE: Native = Native {
    name: "setup",
    aliases: &[],
    enabled: true,
    run: |args, context, cancelled| {
        let clock = Clock {
            deadline: context.timeout.map(|timeout| Instant::now() + timeout),
            cancelled,
        };
        match answer(args, &Root::real(), context, agent_env(), clock) {
            Ok(result) => NativeRun::Done(TaskResult(result)),
            Err(Stop::HandBack(reason)) => NativeRun::Fallback(reason),
            // What the Python path answers when the module outlives the task's `timeout`.
            Err(Stop::TimedOut) => NativeRun::Done(TaskResult::timed_out(
                context.timeout.unwrap_or_default().as_secs(),
            )),
            Err(Stop::Cancelled) => NativeRun::Cancelled,
        }
    },
};

const DEFAULT_FACT_PATH: &str = "/etc/ansible/facts.d";

/// `setup`'s `argument_spec` in ansible-core 2.19.12.
const SPEC: &[ArgSpec] = &[
    ArgSpec {
        name: "gather_subset",
        aliases: &[],
        default: || Value::from(vec!["all"]),
    },
    ArgSpec {
        name: "gather_timeout",
        aliases: &[],
        default: || Value::from(10),
    },
    ArgSpec {
        name: "filter",
        aliases: &[],
        default: || Value::Array(Vec::new()),
    },
    ArgSpec {
        name: "fact_path",
        aliases: &[],
        default: || Value::from(DEFAULT_FACT_PATH),
    },
];

/// The collectors `setup` runs for `min`, its `minimal_gather_subset`.
const MIN: &[&str] = &[
    "apparmor",
    "caps",
    "cmdline",
    "date_time",
    "distribution",
    "dns",
    "env",
    "fips",
    "local",
    "lsb",
    "pkg_mgr",
    "platform",
    "python",
    "selinux",
    "service_mgr",
    "ssh_pub_keys",
    "user",
];

/// The collectors ansible-core 2.19.12 finds for Linux: the name, the fact ids that select it
/// too, the collectors it requires. All but Puppet's, whose name and fact id only ever hand
/// back: named, it is a collector the native does not run, and as an unknown name it is refused
/// the same way; negated or brought in by `all`, it changes nothing the native collects.
const COLLECTORS: &[(&str, &[&str], &[&str])] = &[
    ("apparmor", &[], &[]),
    (
        "caps",
        &["system_capabilities", "system_capabilities_enforced"],
        &[],
    ),
    ("chroot", &["is_chroot"], &[]),
    ("cmdline", &[], &[]),
    ("date_time", &[], &[]),
    (
        "distribution",
        &[
            "distribution_major_version",
            "distribution_release",
            "distribution_version",
            "os_family",
        ],
        &[],
    ),
    ("dns", &[], &[]),
    ("env", &[], &[]),
    ("fibre_channel_wwn", &[], &[]),
    ("fips", &[], &[]),
    (
        "hardware",
        &[
            "devices",
            "mounts",
            "processor",
            "processor_cores",
            "processor_count",
        ],
        &["platform"],
    ),
    ("iscsi", &[], &[]),
    ("loadavg", &[], &[]),
    ("local", &[], &[]),
    ("lsb", &[], &[]),
    (
        "network",
        &[
            "all_ipv4_addresses",
            "all_ipv6_addresses",
            "default_ipv4",
            "default_ipv6",
            "interfaces",
        ],
        &["distribution", "platform"],
    ),
    ("nvme", &[], &[]),
    ("ohai", &[], &[]),
    ("pkg_mgr", &[], &["distribution"]),
    (
        "platform",
        &[
            "architecture",
            "kernel",
            "kernel_version",
            "machine",
            "machine_id",
            "python_version",
            "system",
        ],
        &[],
    ),
    ("python", &[], &[]),
    ("selinux", &[], &[]),
    ("service_mgr", &[], &["distribution", "platform"]),
    (
        "ssh_pub_keys",
        &[
            "ssh_host_key_dsa_public",
            "ssh_host_key_ecdsa_public",
            "ssh_host_key_ed25519_public",
            "ssh_host_key_rsa_public",
            "ssh_host_pub_keys",
        ],
        &[],
    ),
    ("systemd", &[], &[]),
    (
        "user",
        &[
            "effective_group_ids",
            "effective_user_id",
            "real_user_id",
            "user_dir",
            "user_gecos",
            "user_gid",
            "user_id",
            "user_shell",
            "user_uid",
        ],
        &[],
    ),
    (
        "virtual",
        &[
            "virtualization_role",
            "virtualization_tech_guest",
            "virtualization_tech_host",
            "virtualization_type",
        ],
        &[],
    ),
];

/// The collectors the native runs besides `min`, each restricted to part of its facts.
const RESTRICTED: &[&str] = &["hardware", "network"];

/// Fact ids that select a restricted collector for facts the native leaves out.
const LEFT_OUT: &[&str] = &["devices", "mounts"];

/// The agent's own environment, `None` when a name or a value is not UTF-8.
fn agent_env() -> Option<BTreeMap<String, String>> {
    std::env::vars_os()
        .map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// The module's result, or the reason to hand the task back. `agent` is the agent's own
/// environment ([`agent_env`]), which the early `lsb_release` and `ip` guess the module's from.
fn answer(
    args: &Map<String, Value>,
    root: &Root,
    context: &Context,
    agent: Option<BTreeMap<String, String>>,
    clock: Clock,
) -> Result<Map<String, Value>, Stop> {
    let request = request(args)?;
    let facts = collect(&request, root, context, agent, clock)?;
    let mut invocation = invocation(SPEC, args);
    invocation["module_args"]["gather_subset"] = Value::from(request.gather_subset);
    if !matches!(args.get("filter"), None | Some(Value::Null)) {
        invocation["module_args"]["filter"] = Value::from(request.filter);
    }
    let mut result = Map::new();
    result.insert("ansible_facts".into(), Value::Object(facts));
    result.insert("invocation".into(), invocation);
    Ok(result)
}

/// The host as the module would see it.
pub struct Host<'a> {
    pub root: &'a Root,
    /// The module's environment: the one its interpreter starts with, plus the task's.
    pub env: BTreeMap<String, String>,
    pub probe: &'a python::Probe,
    pub clock: Clock<'a>,
}

impl Host<'_> {
    pub fn bin_path(&self, name: &str) -> Option<PathBuf> {
        bin_path(self.root, &self.env, name)
    }

    pub fn run(&self, program: &Path, args: &[&str]) -> Result<Option<(i32, String)>, Stop> {
        run(&self.env, self.clock, program, args)
    }
}

/// `lsb_release -a`, run once for the two readers the reference runs it for: the `distro`
/// library, which finds it along `PATH` only, and the `lsb` collector, which uses
/// `get_bin_path`.
pub struct LsbRelease {
    /// For `distro`: the output when the command exited 0.
    pub distro: Option<String>,
    /// For the `lsb` collector: exit code and output, when the command was found.
    pub collector: Option<(i32, String)>,
}

impl LsbRelease {
    fn run(root: &Root, env: &BTreeMap<String, String>, clock: Clock) -> Result<LsbRelease, Stop> {
        let collector_path = bin_path(root, env, "lsb_release");
        let distro_path = exec_path(root, env, "lsb_release");
        let started = "lsb_release was found and could not be started";
        let collector = match &collector_path {
            Some(path) => Some(run(env, clock, path, &["-a"])?.ok_or(started)?),
            None => None,
        };
        let distro_run = if distro_path == collector_path {
            collector.clone()
        } else {
            match &distro_path {
                Some(path) => Some(run(env, clock, path, &["-a"])?.ok_or(started)?),
                None => None,
            }
        };
        Ok(LsbRelease {
            distro: distro_run.and_then(|(code, out)| (code == 0).then_some(out)),
            collector,
        })
    }
}

/// Runs the native `setup` for `volant-agent --native-facts <gather_subset> [<interpreter>]`,
/// printing the module's result, or the reason it hands back on stderr with status 2.
pub fn print(gather_subset: &str, interpreter: &str) -> i32 {
    let mut args = Map::new();
    args.insert("gather_subset".into(), Value::from(gather_subset));
    let context = Context {
        interpreter: Some(interpreter.to_string()),
        ..Context::default()
    };
    match answer(&args, &Root::real(), &context, agent_env(), unbounded()) {
        Ok(result) => {
            let result = crate::modules::module_result(result);
            println!("{}", Value::Object(result.0));
            0
        }
        Err(Stop::HandBack(reason)) => {
            eprintln!("volant-agent: the native setup hands back: {reason}");
            2
        }
        Err(stop) => {
            eprintln!("volant-agent: the native setup stopped: {stop:?}");
            2
        }
    }
}

#[cfg(test)]
pub mod tests;
