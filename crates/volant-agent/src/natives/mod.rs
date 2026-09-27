// SPDX-License-Identifier: GPL-3.0-or-later
//! Native versions of Python modules: code in the agent that answers a task the controller sent
//! with a Python payload, when the task's arguments stay inside what the native implements.
//!
//! A native receives the arguments the Python module would have received and either answers
//! with the reference module's result or hands the task back. Handing back is only allowed
//! before anything on the host changed: each native decides first, reading only, and acts
//! after. The dispatcher in `modules::run` then runs the payload, which does the real work.

// The natives below read and change a Unix host; elsewhere the agent only builds, for the
// workspace's own checks, and their tasks go to the Python module.
#[cfg(unix)]
pub mod common;

#[cfg(unix)]
mod apt;
/// The native `apt` reads a Linux host's files; elsewhere the task goes to the Python module.
#[cfg(not(unix))]
mod apt {
    use crate::natives::{Native, NativeRun};

    pub const NATIVE: Native = Native {
        name: "apt",
        aliases: &[],
        enabled: false,
        run: |_, _, _| NativeRun::Fallback("the native apt reads a Linux host".into()),
    };
}
#[cfg(unix)]
mod copy;
/// The native `copy` moves files on a Linux host; elsewhere the task goes to the Python module.
#[cfg(not(unix))]
mod copy {
    use crate::natives::{Native, NativeRun};

    pub const NATIVE: Native = Native {
        name: "copy",
        aliases: &[],
        enabled: false,
        run: |_, _, _| NativeRun::Fallback("the native copy moves files on a Linux host".into()),
    };
}
#[cfg(unix)]
mod file;
mod group;
#[cfg(unix)]
mod lineinfile;
mod package_facts;
mod service_facts;
#[cfg(unix)]
pub mod setup;
/// The agent is only ever uploaded to Linux hosts; elsewhere it builds for the workspace's own
/// checks, and `setup` goes to the Python module.
#[cfg(not(unix))]
pub mod setup {
    use crate::natives::{Native, NativeRun};

    pub const NATIVE: Native = Native {
        name: "setup",
        aliases: &[],
        enabled: false,
        run: |_, _, _| NativeRun::Fallback("the native setup reads a Linux host".into()),
    };

    pub fn print(_: &str, _: &str) -> i32 {
        eprintln!("volant-agent: the native setup reads a Linux host");
        2
    }
}
#[cfg(unix)]
mod stat;
#[cfg(unix)]
mod systemd;
mod user;

use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use crate::modules::Context;

/// What a native did with a task.
pub enum NativeRun {
    #[cfg_attr(
        not(any(unix, feature = "test-natives")),
        expect(dead_code, reason = "returned by the natives of a Linux build")
    )]
    Done(TaskResult),
    /// This case is outside the native subset. Returned before anything on the host changed;
    /// the dispatcher then runs the task's Python payload, which does the real work.
    Fallback(String),
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "returned by the natives of a Linux build")
    )]
    Cancelled,
}

pub type NativeFn = fn(&Map<String, Value>, &Context, &dyn Fn() -> bool) -> NativeRun;

pub struct Native {
    /// The reference's short name: `stat`, `systemd`, ...
    pub name: &'static str,
    /// Other names the reference gives the same module: `systemd_service` for `systemd`.
    pub aliases: &'static [&'static str],
    /// Turned on only in the change whose golden shows the native identical to the reference.
    pub enabled: bool,
    pub run: NativeFn,
}

/// One entry per native, enabled or not.
pub const NATIVES: &[Native] = &[
    setup::NATIVE,
    #[cfg(unix)]
    stat::NATIVE,
    #[cfg(unix)]
    file::NATIVE,
    copy::NATIVE,
    #[cfg(unix)]
    lineinfile::NATIVE,
    #[cfg(unix)]
    systemd::NATIVE,
    apt::NATIVE,
    user::NATIVE,
    group::NATIVE,
    package_facts::NATIVE,
    service_facts::NATIVE,
    #[cfg(feature = "test-natives")]
    echo::NATIVE,
    #[cfg(feature = "test-natives")]
    echo::DISABLED,
];

/// The native going by `name` or one of its aliases, enabled or not.
pub fn find(name: &str) -> Option<&'static Native> {
    NATIVES
        .iter()
        .find(|native| native.name == name || native.aliases.contains(&name))
}

/// Every name, aliases included, under which this agent runs a native: what `Ready.natives`
/// tells the controller.
pub fn enabled_names() -> Vec<String> {
    NATIVES
        .iter()
        .filter(|native| native.enabled)
        .flat_map(|native| std::iter::once(native.name).chain(native.aliases.iter().copied()))
        .map(str::to_string)
        .collect()
}

/// Runs `native`, turning a panic into a failed task so the agent lives on to run the rest of
/// the batch. Failed rather than handed back: the panic may come after the native changed the
/// host, and the payload must never run over a half-made change.
///
/// Only as good as the build's panic strategy: under `panic = "abort"` the process ends before
/// this sees anything, which is why no profile of the workspace sets it.
pub fn run(
    native: &Native,
    args: &Map<String, Value>,
    context: &Context,
    cancelled: &dyn Fn() -> bool,
) -> NativeRun {
    catch_unwind(AssertUnwindSafe(|| (native.run)(args, context, cancelled)))
        .unwrap_or_else(|_| NativeRun::Done(TaskResult::failed_with("native module panicked")))
}

/// A native that exists only to exercise the dispatcher before any real one does. Built only
/// with the `test-natives` feature, which the agent's own tests turn on and a release never does.
#[cfg(feature = "test-natives")]
mod echo {
    use serde_json::{Map, Value};
    use volant_protocol::TaskResult;

    use super::{Native, NativeRun};
    use crate::modules::Context;

    pub const NATIVE: Native = Native {
        name: "volant_echo",
        aliases: &[],
        enabled: true,
        run,
    };

    /// A native that stays disabled, for the dispatcher's "never consulted" case: no plan ever
    /// enables it, and it would answer like `volant_echo` if it were consulted.
    pub const DISABLED: Native = Native {
        name: "volant_disabled",
        aliases: &[],
        enabled: false,
        run,
    };

    /// Answers `{"echo": <args>}`, leaving `changed` to the dispatcher, and with `read_src` also
    /// the content of the file named by `src`. Hands the task back when asked to, with the reason
    /// it was given, and panics when asked to.
    fn run(args: &Map<String, Value>, _: &Context, _: &dyn Fn() -> bool) -> NativeRun {
        if let Some(reason) = args.get("fallback").and_then(Value::as_str) {
            return NativeRun::Fallback(reason.to_string());
        }
        assert!(
            args.get("panic").is_none(),
            "volant_echo was asked to panic"
        );
        let mut result = Map::new();
        result.insert("echo".into(), Value::Object(args.clone()));
        if args.contains_key("read_src") {
            let src = args.get("src").and_then(Value::as_str).unwrap_or_default();
            result.insert(
                "src_content".into(),
                std::fs::read_to_string(src).map_or(Value::Null, Value::String),
            );
        }
        NativeRun::Done(TaskResult(result))
    }
}

#[cfg(test)]
mod tests {
    use volant_protocol::modules::NATIVE_CANDIDATES;

    use super::*;

    /// The protocol's list of candidates and the agent's table name the same modules.
    ///
    /// What would make this red: a native added here and not to the candidates, which the
    /// controller would never send it; or a candidate with no file, which leaves nothing to
    /// enable when its golden is ready.
    #[test]
    fn every_candidate_has_a_native_and_every_native_is_a_candidate() {
        for name in NATIVE_CANDIDATES {
            assert!(find(name).is_some(), "{name} has no native");
        }
        for native in NATIVES.iter().filter(|n| !n.name.starts_with("volant_")) {
            for name in std::iter::once(native.name).chain(native.aliases.iter().copied()) {
                assert!(NATIVE_CANDIDATES.contains(&name), "{name} is no candidate");
            }
        }
    }

    /// The modules page says which natives run from the protocol's list; this agent's table is
    /// the truth.
    ///
    /// What would make this red: a native enabled here and not named on the page, or the page
    /// naming one this agent does not run.
    #[test]
    #[cfg(unix)]
    fn the_modules_page_names_the_natives_this_agent_runs() {
        use volant_protocol::modules::NATIVE_ENABLED;

        let mut ours = enabled_names();
        ours.retain(|name| name != "volant_echo");
        ours.sort();
        let mut page = NATIVE_ENABLED.to_vec();
        page.sort_unstable();
        assert_eq!(ours, page);
    }

    /// A hand-back provoked with a secret-looking value, for the reason guard below: the case,
    /// the secret, and the reason the native gave (`None` when it did not hand back). A native
    /// whose input is a host file or a command's output adds its cases from its own fake host.
    #[cfg(target_os = "linux")]
    pub(super) type Probe = (&'static str, &'static str, Option<String>);

    /// A hand-back reason reaches the profile and its JSON file, which get shared: it names the
    /// argument and why the native leaves it, never the value, a file's content or a command's
    /// output, any of which can be a secret. One row per hand-back site that once quoted one.
    ///
    /// What would make this red: a reason that quotes a `regexp`, a `validate` program, a mode,
    /// a state, an account, an id, a unit or package name the task gave, an apt source's URI,
    /// suite, component, type, option value or line, a unit line, or a `PATH` entry.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_hand_back_reason_never_quotes_an_argument_value() {
        use serde_json::json;

        let scratch = common::golden::Scratch::new("reason-values");
        let file = scratch.path("config.yaml");
        std::fs::write(&file, "token: K3S0SECRET0E\n").unwrap();
        std::fs::create_dir(scratch.path(".tmp")).unwrap();
        let context = Context {
            remote_tmp: scratch.path(".tmp"),
            ..Context::default()
        };
        // Natives that take the value as an argument, run as the agent runs them.
        let by_args = [
            (
                "lineinfile",
                json!({"path": file, "regexp": "^token: K3S0SECRET0A**", "line": "x"}),
                "K3S0SECRET0A",
            ),
            (
                "lineinfile",
                json!({"path": file, "regexp": "K3S0SECRET0B*+", "line": "x"}),
                "K3S0SECRET0B",
            ),
            (
                "lineinfile",
                json!({"path": file, "regexp": "(?<K3S0SECRET0C>x)", "line": "x"}),
                "K3S0SECRET0C",
            ),
            (
                "lineinfile",
                json!({"path": file, "insertafter": "K3S0SECRET0D**", "line": "x"}),
                "K3S0SECRET0D",
            ),
            (
                "lineinfile",
                json!({
                    "path": file,
                    "search_string": "token: K3S0SECRET0E",
                    "line": "token: K3S0SECRET0F",
                    "validate": "/nonexistent/K3S0SECRET0G %s",
                }),
                "K3S0SECRET0",
            ),
            (
                "copy",
                json!({"src": file, "dest": scratch.path("copied"), "mode": "+75318642"}),
                "75318642",
            ),
            (
                "file",
                json!({"path": file, "state": "K3S0SECRET0H"}),
                "K3S0SECRET0H",
            ),
            (
                "file",
                json!({"path": file, "owner": "+4815162342"}),
                "4815162342",
            ),
            (
                "user",
                json!({"name": "volant-reason-probe", "group": "+4815162343"}),
                "4815162343",
            ),
            (
                "user",
                json!({"name": "volant-reason-probe", "groups": ["+4815162344"]}),
                "4815162344",
            ),
            (
                "group",
                json!({"name": "volant-reason-probe", "gid": "K3S0SECRET0L"}),
                "K3S0SECRET0L",
            ),
            (
                "group",
                json!({"name": "volant-reason-probe", "state": "K3S0SECRET0M"}),
                "K3S0SECRET0M",
            ),
            ("group", json!({"name": "K3S0SECRET0N:x"}), "K3S0SECRET0N"),
            (
                "systemd",
                json!({"name": "volant-reason-probe.service", "state": "K3S0SECRET0O"}),
                "K3S0SECRET0O",
            ),
            (
                "systemd",
                json!({"name": "K3S0SECRET0P.service", "masked": true}),
                "K3S0SECRET0P",
            ),
            (
                "systemd",
                json!({"name": "K3S0SECRET0Q*.service", "state": "started"}),
                "K3S0SECRET0Q",
            ),
            (
                "setup",
                json!({"gather_subset": ["k3s0secret0k"]}),
                "k3s0secret0k",
            ),
        ];
        let mut probes: Vec<Probe> = by_args
            .into_iter()
            .map(|(name, args, secret)| {
                let ran = run(
                    find(name).unwrap(),
                    args.as_object().unwrap(),
                    &context,
                    &|| false,
                );
                let reason = match ran {
                    NativeRun::Fallback(reason) => Some(reason),
                    _ => None,
                };
                (name, secret, reason)
            })
            .collect();
        // Natives that read the value from a host file or a command's output, on a fake host.
        probes.extend(apt::tests::secret_probes());
        probes.extend(package_facts::secret_probes());
        probes.extend(service_facts::secret_probes());

        let mut leaks = Vec::new();
        for (case, secret, reason) in probes {
            match reason {
                Some(reason) if reason.contains(secret) => leaks.push(format!("{case}: {reason}")),
                Some(_) => {}
                None => leaks.push(format!("{case} did not hand back on {secret}")),
            }
        }
        assert!(leaks.is_empty(), "reasons quote a value: {leaks:#?}");
    }
}
