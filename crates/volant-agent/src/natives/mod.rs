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

/// The natives that only read the host. The reference's own modules run unchanged in check mode
/// and show no diff, so these answer a task in either mode as they answer any other.
const READ_ONLY: [&str; 4] = ["package_facts", "service_facts", "setup", "stat"];

/// Why `native` hands a task in check or diff mode back to its Python module, or `None` when it
/// answers it. A native that changes the host knows neither what it would have changed without
/// changing it nor how the reference words the diff, and the Python module knows both.
pub fn mode_hand_back(native: &Native, check_mode: bool, diff: bool) -> Option<&'static str> {
    if READ_ONLY.contains(&native.name) {
        None
    } else if check_mode {
        Some("the task runs in check mode")
    } else if diff {
        Some("the task shows a diff")
    } else {
        None
    }
}

const _: () = assert!(
    cfg!(panic = "unwind"),
    "natives::run needs an unwinding build to turn a native's panic into a failed task"
);

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
mod tests;
