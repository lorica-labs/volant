// SPDX-License-Identifier: GPL-3.0-or-later
//! Native versions of Python modules: code in the agent that answers a task the controller sent
//! with a Python payload, when the task's arguments stay inside what the native implements.
//!
//! A native receives the arguments the Python module would have received and either answers
//! with the reference module's result or hands the task back. Handing back is only allowed
//! before anything on the host changed: each native decides first, reading only, and acts
//! after. The dispatcher in `modules::run` then runs the payload, which does the real work.

pub mod common;

mod apt;
mod copy;
mod file;
mod group;
mod lineinfile;
mod package_facts;
mod service_facts;
mod setup;
mod stat;
mod systemd;
mod user;

use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::{Map, Value};
use volant_protocol::TaskResult;

use crate::modules::Context;

/// What a native did with a task.
pub enum NativeRun {
    #[cfg_attr(
        not(feature = "test-natives"),
        expect(dead_code, reason = "returned by the first native that is written")
    )]
    Done(TaskResult),
    /// This case is outside the native subset. Returned before anything on the host changed;
    /// the dispatcher then runs the task's Python payload, which does the real work.
    Fallback(String),
    #[expect(
        dead_code,
        reason = "returned by the first native that honours a cancel"
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
    stat::NATIVE,
    file::NATIVE,
    copy::NATIVE,
    lineinfile::NATIVE,
    systemd::NATIVE,
    apt::NATIVE,
    user::NATIVE,
    group::NATIVE,
    package_facts::NATIVE,
    service_facts::NATIVE,
    #[cfg(feature = "test-natives")]
    echo::NATIVE,
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

/// Runs `native`, turning a panic into a hand-back so the payload still runs and the rest of the
/// batch with it.
///
/// Only as good as the build's panic strategy: under `panic = "abort"` the process ends before
/// this sees anything.
pub fn run(
    native: &Native,
    args: &Map<String, Value>,
    context: &Context,
    cancelled: &dyn Fn() -> bool,
) -> NativeRun {
    catch_unwind(AssertUnwindSafe(|| (native.run)(args, context, cancelled)))
        .unwrap_or_else(|_| NativeRun::Fallback("native module panicked".into()))
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

    /// Answers `{"changed": false, "echo": <args>}`, and with `read_src` also the content of the
    /// file named by `src`. Hands the task back when asked to, with the reason it was given, and
    /// panics when asked to.
    fn run(args: &Map<String, Value>, _: &Context, _: &dyn Fn() -> bool) -> NativeRun {
        if let Some(reason) = args.get("fallback").and_then(Value::as_str) {
            return NativeRun::Fallback(reason.to_string());
        }
        assert!(
            args.get("panic").is_none(),
            "volant_echo was asked to panic"
        );
        let mut result = Map::new();
        result.insert("changed".into(), Value::Bool(false));
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
        for native in NATIVES.iter().filter(|n| n.name != "volant_echo") {
            for name in std::iter::once(native.name).chain(native.aliases.iter().copied()) {
                assert!(NATIVE_CANDIDATES.contains(&name), "{name} is no candidate");
            }
        }
    }
}
