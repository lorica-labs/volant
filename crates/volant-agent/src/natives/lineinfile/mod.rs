// SPDX-License-Identifier: GPL-3.0-or-later
//! `lineinfile`, answered in the agent as `ansible/modules/lineinfile.py` of ansible-core 2.19.12
//! answers it.
//!
//! The native reads the file and works out its new lines in memory first, with the reference's
//! own rules: the file read as bytes and split after each `\n`, `regexp` searched line by line
//! with the last match winning unless `firstmatch`, `insertafter`/`insertbefore` read the same
//! way, `search_string` as a plain substring. Only then does it act, in the reference's order:
//! parent directories, backup, temporary file, `validate`, rename, owner, group and mode. Every
//! hand-back comes before the first of those. Outside the subset the Python module answers:
//! `backrefs`, an expression Rust's `regex` has no exact counterpart for, SELinux, `attributes`,
//! a link or a special file as the path, extended attributes on the file, a task `environment`.

#[cfg(all(test, target_os = "linux"))]
use std::ffi::CString;

use regex::bytes::Regex;
use serde_json::{Map, Value, json};

use super::common::{
    Account, ArgSpec, ModeError, add_path_info, bool_param, check, check_names, clock, flag,
    group_account, module_args, native_run, null, owner_account, parse_mode, path_param,
    selinux_enabled, str_param,
};
use super::runtime::{Clock, Stop as Halt};
use super::{Native, NativeRun};
use crate::modules::Context;

mod edit;
mod py_regex;
mod write;

use edit::edit;
use py_regex::compile;
#[cfg(all(test, target_os = "linux"))]
use write::write_changes;

pub const NATIVE: Native = Native {
    name: "lineinfile",
    aliases: &[],
    enabled: true,
    run,
};

/// `lineinfile`'s own options, then `add_file_common_args`.
const SPEC: &[ArgSpec] = &[
    ArgSpec {
        name: "path",
        aliases: &["dest", "destfile", "name"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "state",
        aliases: &[],
        default: || json!("present"),
    },
    ArgSpec {
        name: "regexp",
        aliases: &["regex"],
        default: || Value::Null,
    },
    null("search_string"),
    ArgSpec {
        name: "line",
        aliases: &["value"],
        default: || Value::Null,
    },
    null("insertafter"),
    null("insertbefore"),
    flag("backrefs", false),
    flag("create", false),
    flag("backup", false),
    flag("firstmatch", false),
    null("validate"),
    null("mode"),
    null("owner"),
    null("group"),
    null("seuser"),
    null("serole"),
    null("selevel"),
    null("setype"),
    ArgSpec {
        name: "attributes",
        aliases: &["attr"],
        default: || Value::Null,
    },
    flag("unsafe_writes", false),
];

/// The reference's `mutually_exclusive`, counted by presence as `AnsibleModule` counts it.
const EXCLUSIVE: &[[&str; 2]] = &[
    ["insertbefore", "insertafter"],
    ["regexp", "search_string"],
    ["backrefs", "search_string"],
];

/// Options whose every value other than `null` is outside the native.
const UNSUPPORTED: &[&str] = &["attributes", "seuser", "serole", "selevel", "setype"];

/// Why the answer stopped short of a result.
enum Stop {
    /// The reference's `fail_json`, with these keys.
    Fail(Map<String, Value>),
    /// Outside the subset. Only ever returned before anything on the host changed.
    Back(String),
    /// The task's `timeout` ran out, or the controller cancelled it.
    Clock(Halt),
}

impl From<String> for Stop {
    fn from(reason: String) -> Self {
        Stop::Back(reason)
    }
}

impl From<&str> for Stop {
    fn from(reason: &str) -> Self {
        Stop::Back(reason.to_string())
    }
}

impl From<Halt> for Stop {
    fn from(halt: Halt) -> Self {
        match halt {
            Halt::HandBack(reason) => Stop::Back(reason),
            other => Stop::Clock(other),
        }
    }
}

/// What the task asks for, read and checked before anything is touched.
struct Request<'a> {
    path: &'a str,
    present: bool,
    regexp: Option<Regex>,
    search_string: Option<&'a [u8]>,
    line: Option<&'a str>,
    insertafter: Option<&'a str>,
    insertbefore: Option<&'a str>,
    create: bool,
    backup: bool,
    firstmatch: bool,
    validate: Option<&'a str>,
    owner: Option<Account>,
    group: Option<Account>,
    mode: Option<Value>,
}

fn run(args: &Map<String, Value>, context: &Context, cancelled: &dyn Fn() -> bool) -> NativeRun {
    native_run(answer(args, context, clock(context, cancelled)), context)
}

fn answer(
    args: &Map<String, Value>,
    context: &Context,
    clock: Clock,
) -> Result<Map<String, Value>, Halt> {
    if !cfg!(target_os = "linux") {
        return Err("the native answers on Linux only".into());
    }
    if !context.environment.is_empty() {
        return Err("the task sets an environment".into());
    }
    if std::env::var_os("ANSIBLE_UNSAFE_WRITES").is_some() {
        return Err("ANSIBLE_UNSAFE_WRITES is set".into());
    }
    if selinux_enabled() {
        return Err("SELinux is enabled".into());
    }
    check(clock)?;
    check_names(SPEC, args)?;
    for pair in EXCLUSIVE {
        if pair.iter().all(|name| given(args, name)) {
            return Err(format!("{} and {} are mutually exclusive", pair[0], pair[1]).into());
        }
    }
    let params = module_args(SPEC, args);
    let request = request(&params, clock)?;
    let mut result = match edit(&request, context, clock) {
        Ok(result) => result,
        Err(Stop::Fail(mut fail)) => {
            fail.insert("failed".into(), Value::Bool(true));
            fail
        }
        Err(Stop::Back(reason)) => return Err(Halt::HandBack(reason)),
        Err(Stop::Clock(halt)) => return Err(halt),
    };
    add_path_info(&mut result, clock)?;
    result.insert("invocation".into(), json!({"module_args": params}));
    Ok(result)
}

/// Whether `name` was given under any of its spellings.
fn given(args: &Map<String, Value>, name: &str) -> bool {
    SPEC.iter()
        .filter(|arg| arg.name == name)
        .flat_map(|arg| std::iter::once(arg.name).chain(arg.aliases.iter().copied()))
        .any(|spelling| args.contains_key(spelling))
}

/// The parameters as `AnsibleModule` would validate them. Whatever it would convert, warn about
/// or refuse goes back to the Python module, which says it in its own words.
fn request<'a>(params: &'a Map<String, Value>, clock: Clock) -> Result<Request<'a>, Halt> {
    if let Some(name) = UNSUPPORTED.iter().find(|name| !params[**name].is_null()) {
        return Err(format!("{name} is set").into());
    }
    for name in [
        "backrefs",
        "create",
        "backup",
        "firstmatch",
        "unsafe_writes",
    ] {
        bool_param(params, name)?;
    }
    if bool_param(params, "unsafe_writes")? {
        return Err("unsafe_writes is set".into());
    }
    let present = match str_param(params, "state")? {
        Some("present") => true,
        Some("absent") => false,
        _ => return Err("state is not one of its choices".into()),
    };
    let text = |name| str_param(params, name);
    let (regexp, search_string, line) = (text("regexp")?, text("search_string")?, text("line")?);
    let (insertafter, insertbefore, validate) = (
        text("insertafter")?,
        text("insertbefore")?,
        // `valid = not validate`: an empty one is no `validate` at all.
        text("validate")?.filter(|validate| !validate.is_empty()),
    );
    let path = path_param(params, "path")?;
    let mode = Some(params["mode"].clone()).filter(|mode| !mode.is_null());
    if let Some(mode) = &mode
        && let Err(ModeError::Unsupported(why)) = parse_mode(mode, 0, false)
    {
        return Err(why.into());
    }
    // The reference warns about an empty `regexp` or `search_string`; an empty `insertafter` or
    // `insertbefore` is compiled as an expression yet read as false.
    for (name, value) in [
        ("regexp", regexp),
        ("search_string", search_string),
        ("insertafter", insertafter),
        ("insertbefore", insertbefore),
    ] {
        if value == Some("") {
            return Err(format!("{name} is empty").into());
        }
    }
    if bool_param(params, "backrefs")? {
        return Err("backrefs is set".into());
    }
    Ok(Request {
        path,
        present,
        regexp: regexp
            .map(|pattern| compile("regexp", pattern))
            .transpose()?,
        search_string: search_string.map(str::as_bytes),
        line,
        insertafter,
        insertbefore,
        create: bool_param(params, "create")?,
        backup: bool_param(params, "backup")?,
        firstmatch: bool_param(params, "firstmatch")?,
        validate,
        owner: text("owner")?
            .map(|owner| owner_account(owner, clock))
            .transpose()?,
        group: text("group")?
            .map(|group| group_account(group, clock))
            .transpose()?,
        mode,
    })
}

/// A `fail_json(msg=msg)`, with `rc` when the reference gives one.
fn fail(msg: String, rc: Option<i32>) -> Stop {
    let mut fail = message(msg);
    if let Some(rc) = rc {
        fail.insert("rc".into(), rc.into());
    }
    Stop::Fail(fail)
}

fn message(msg: String) -> Map<String, Value> {
    let mut fail = Map::new();
    fail.insert("msg".into(), msg.into());
    fail
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
