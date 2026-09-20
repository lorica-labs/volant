// SPDX-License-Identifier: GPL-3.0-or-later
//! Native modules. Each one takes Ansible's argument names and returns Ansible's result shape.
//! The list of names lives in `volant_protocol::modules`; this table maps each name to code.

pub mod command;
mod glob;
pub mod raw;
pub mod shell;

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Map, Value};
use volant_protocol::modules::{ArgStatus, ModuleSpec, arg_bool, short_name};
use volant_protocol::{Task, TaskResult};

/// Outcome of running one task.
pub enum Run {
    Done(TaskResult),
    /// The controller cancelled the batch while this task was running.
    Cancelled,
}

/// What a module is given besides its own arguments: everything the task keywords decided and
/// the module itself never reads out of `args`.
///
/// One struct rather than a growing argument list, so a keyword added to it reaches every module
/// through one signature instead of touching each of them again.
#[derive(Debug, Default, Clone)]
pub struct Context {
    /// Seconds the module gets before it is killed (Ansible's `timeout`).
    pub timeout: Option<Duration>,
    /// Variables the module's process runs with, added to the ones the agent inherited
    /// (Ansible's `environment`).
    pub environment: BTreeMap<String, String>,
}

/// The function signature every native module implements.
type ModuleFn = fn(&Map<String, Value>, &Context, &dyn Fn() -> bool) -> Run;

/// A native module: its shared spec and the function that runs it.
pub struct Module {
    pub spec: &'static ModuleSpec,
    pub run: ModuleFn,
}

pub const MODULES: &[Module] = &[command::MODULE, raw::MODULE, shell::MODULE];

/// Runs a task with the native module that matches its name.
pub fn run(task: &Task, cancelled: &dyn Fn() -> bool) -> Run {
    let name = short_name(&task.module);
    let context = Context {
        timeout: task.timeout.map(Duration::from_secs),
        environment: task.environment.clone(),
    };
    match MODULES.iter().find(|m| m.spec.name == name) {
        Some(module) => match unsupported_parameters(module.spec, &task.args)
            .or_else(|| refused_argument(module.spec, &task.args))
        {
            Some(msg) => Run::Done(TaskResult::failed_with(msg)),
            None => (module.run)(&task.args, &context, cancelled),
        },
        None => Run::Done(TaskResult::failed_with(format!(
            "The module {name} is not available on the agent yet"
        ))),
    }
}

/// The reference's own answer for an argument it does not have, word for word, or `None` when
/// every argument is one it has.
///
/// This is the second of the two refusals, and the one that belongs to the reference rather than
/// to this engine. An argument the reference has and this release drops is named by the
/// pre-flight, before anything connects; an argument **nobody** has is a mistake the reference
/// already answers, so it is answered here, at the same moment and in the same words - measured
/// on ansible-core 2.19.12 through `args:`, which is the path that validates at all.
///
/// The module is named `ansible.legacy.command` for `shell` too, because the reference executes
/// `shell` with the `command` module. `raw` reaches no module and validates nothing, which is why
/// the name is what decides here rather than the argument list being empty.
fn unsupported_parameters(spec: &ModuleSpec, args: &Map<String, Value>) -> Option<String> {
    let module = spec.validated_as?;
    let mut unknown: Vec<&str> = args
        .keys()
        .map(String::as_str)
        .filter(|key| !spec.args.iter().any(|a| a.name == *key))
        .collect();
    if unknown.is_empty() {
        return None;
    }
    unknown.sort_unstable();
    let supported: Vec<&str> = spec.args.iter().map(|a| a.name).collect();
    Some(format!(
        "Unsupported parameters for ({module}) module: {}. Supported parameters include: {}.",
        unknown.join(", "),
        supported.join(", ")
    ))
}

/// The pre-flight's refusal, arriving late because the value did not exist when the pre-flight
/// ran, or `None` when nothing in the task asks for what this release cannot do.
///
/// This is the **first** of the two refusals, not the second: an argument the reference has and
/// this release does not act on. `check_arguments` names it before the first connection whenever
/// it can read the value, and it cannot read a template - which is rendered per host, long after
/// the pre-flight is over - so it lets one through rather than refusing on a guess. Here the value
/// exists, so the same list gives the same answer, and the argument no longer reaches a module
/// that would drop it and report the task green. It also answers for a task spliced in by a
/// dynamic include, which no pre-flight pass ever sees.
///
/// The sentence says where it came from, so the two refusals stay apart on the operator's screen:
/// this one is a release that is missing something, the other is a name that exists nowhere.
fn refused_argument(spec: &ModuleSpec, args: &Map<String, Value>) -> Option<String> {
    args.iter().find_map(|(key, value)| {
        let ArgStatus::Refused(guard) = spec.args.iter().find(|a| a.name == key.as_str())?.status
        else {
            return None;
        };
        if let Some(refused) = guard
            && arg_bool(value) != Some(refused)
        {
            return None;
        }
        let with = guard.map(|v| format!(" with '{v}'")).unwrap_or_default();
        Some(format!(
            "argument '{key}' is not supported yet{with} on '{}'. The host renders this value, so the check before the run could not read it.",
            spec.name
        ))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use volant_protocol::modules::{COMMAND, NATIVE_MODULES, RAW, SHELL};

    use super::*;

    #[test]
    fn every_registered_module_is_implemented_and_nothing_more() {
        let registered: Vec<&str> = NATIVE_MODULES.iter().map(|m| m.name).collect();
        let implemented: Vec<&str> = MODULES.iter().map(|m| m.spec.name).collect();
        assert_eq!(implemented, registered);
    }

    /// The sentence and the module name are the reference's, not this engine's, and `shell` is
    /// answered under `command` because that is the module the reference runs it with. `raw`
    /// validates nothing at all: measured, a `raw` task carrying an argument no engine has runs
    /// and reports `changed`.
    ///
    /// What would make this red: the message rewritten in this engine's words, which sends an
    /// operator looking for a Volant bug instead of a typo; `shell` answering under its own name,
    /// which no reference output contains; or `raw` starting to validate, which fails a playbook
    /// the reference runs.
    #[test]
    fn an_argument_the_reference_does_not_have_gets_the_reference_s_sentence() {
        let args = |v: Value| v.as_object().unwrap().clone();
        for spec in [&COMMAND, &SHELL] {
            let msg = unsupported_parameters(
                spec,
                &args(json!({"_raw_params": "/bin/true", "no_such_arg": 1})),
            )
            .expect("an argument nobody has is refused");
            assert_eq!(
                msg,
                "Unsupported parameters for (ansible.legacy.command) module: no_such_arg. Supported parameters include: _raw_params, _uses_shell, argv, chdir, cmd, creates, executable, expand_argument_vars, removes, stdin, stdin_add_newline, strip_empty_ends."
            );
            assert_eq!(
                unsupported_parameters(spec, &args(json!({"_raw_params": "x", "chdir": "/tmp"}))),
                None,
                "an argument the reference has passes"
            );
        }
        assert_eq!(
            unsupported_parameters(&RAW, &args(json!({"no_such_arg": 1}))),
            None,
            "raw takes any argument without looking at it"
        );
    }

    /// The pre-flight refuses `expand_argument_vars: true` before the first connection, and lets
    /// `expand_argument_vars: "{{ flag }}"` through because it cannot read a template. The value
    /// exists here, so the refusal fires here instead of the argument being dropped in silence -
    /// which is the accepted-then-ignored failure the registry exists to close, arriving inside
    /// the registry's own subject matter.
    ///
    /// It reads the value the way the reference reads a `type='bool'` argument, so the refusal
    /// covers every spelling of the value it refuses, and the value that asks for what this
    /// release already does still runs.
    ///
    /// What would make this red: nothing checking the rendered value, which prints `$HOME`,
    /// reports `changed` and exits 0 where the reference prints the home directory; or the
    /// refusal firing on the name, which fails a task that ran identically under both engines.
    #[test]
    fn a_refused_argument_whose_value_arrives_late_fails_the_task() {
        let args = |v: Value| v.as_object().unwrap().clone();
        for spec in [&COMMAND, &SHELL] {
            for refused in [json!(true), json!("true"), json!("YES"), json!(1)] {
                assert_eq!(
                    refused_argument(
                        spec,
                        &args(json!({"_raw_params": "/bin/echo $HOME", "expand_argument_vars": refused})),
                    )
                    .as_deref(),
                    Some(
                        format!(
                            "argument 'expand_argument_vars' is not supported yet with 'true' on '{}'. The host renders this value, so the check before the run could not read it.",
                            spec.name
                        )
                        .as_str()
                    ),
                    "{refused}"
                );
            }
            for allowed in [json!(false), json!("no"), json!(0), json!("maybe")] {
                assert_eq!(
                    refused_argument(
                        spec,
                        &args(json!({"expand_argument_vars": allowed.clone()})),
                    ),
                    None,
                    "{allowed}"
                );
            }
            assert_eq!(
                refused_argument(spec, &args(json!({"_raw_params": "x", "chdir": "/tmp"}))),
                None,
                "an argument this release honours is not refused"
            );
        }
        assert_eq!(
            refused_argument(&RAW, &args(json!({"expand_argument_vars": true}))),
            None,
            "raw has no registry entry for it, and the reference takes it without looking"
        );
    }
}
