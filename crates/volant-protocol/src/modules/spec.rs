// SPDX-License-Identifier: GPL-3.0-or-later

//! What a module spec says about each argument: the types and `arg_bool`.

use serde_json::Value;

/// What one module does with one of its arguments.
///
/// A module being in [`NATIVE_MODULES`](super::NATIVE_MODULES) says it runs; it says nothing about the rest of its API,
/// and an argument accepted and then dropped reports success having done something other than
/// what the playbook asked for. The status is per argument so that neither answer is a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgStatus {
    /// Read, and the value decides what runs.
    ///
    /// It says the argument is read, not that it is coerced the way the reference coerces it.
    /// `chdir`, `creates` and `removes` are `type='path'` there, which expands `~` and `$VAR`
    /// before the value is used, and this release takes the value as it is written.
    Honoured,
    /// The reference accepts the name on this module and acts on it nowhere, which is what this
    /// release does with it too - `executable` on `command`, where no shell runs for it to
    /// select. Nothing to promise and nothing to refuse, so it is out of the generated page.
    Inert,
    /// The reference has it and this release does not do what it asks for. Named by the
    /// pre-flight, before the first connection, rather than accepted and forgotten.
    ///
    /// The value carried is the one that is refused, the way `check_mode` is refused for `true`
    /// and runs for `false`: the other value asks for what this release already does, and
    /// refusing that would stop a playbook the two engines agreed on. `None` refuses the name
    /// whatever the value says.
    Refused(Option<bool>),
}

/// A value read the way the reference reads an argument declared `type='bool'`.
///
/// Measured on ansible-core 2.19.12, whose own refusal lists the spellings it takes: `0, 1, 'n',
/// 'on', 'true', 'f', 'false', 'y', 'yes', 'no', '0', '1', 't', 'off'`, and a string outside that
/// set fails the task there. It normalises with `.lower().strip()`, so the value is case folded
/// **and** trimmed before it is matched: a trailing newline off a `lookup('file')`, or the space a
/// template leaves behind, reads the same there as the bare word. `Value::as_bool` sees none of
/// them, so an argument written `"false"` - which a whole-expression template produces on its own
/// - used to read back as the default and do the opposite of what it says.
///
/// This is not the set the YAML scalar resolver uses, and the two must not be merged: that one
/// has no `y`, `n`, `t` or `f`, and matches three fixed capitalisations where this one folds
/// case. The `bool` filter has a third set of its own. Three readers, three measurements, one
/// place each.
pub fn arg_bool(value: &Value) -> Option<bool> {
    let text = match value {
        Value::Bool(b) => return Some(*b),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_ascii_lowercase(),
        _ => return None,
    };
    match text.as_str() {
        "y" | "yes" | "on" | "1" | "1.0" | "true" | "t" => Some(true),
        "n" | "no" | "off" | "0" | "0.0" | "false" | "f" => Some(false),
        _ => None,
    }
}

/// One argument of one module, and what this release does with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleArg {
    pub name: &'static str,
    pub status: ArgStatus,
}

/// One native module: its Ansible name and how the controller parses its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleSpec {
    /// Short name, without the `ansible.builtin.` prefix.
    pub name: &'static str,
    /// The task's string form is a command line kept whole as `_raw_params`,
    /// instead of `key=value` pairs.
    pub free_form: bool,
    /// One line for the generated documentation.
    pub summary: &'static str,
    /// Every argument the reference accepts for this module, sorted, with what this release does
    /// with each. Empty for a module whose arguments nothing here checks yet.
    pub args: &'static [ModuleArg],
    /// The name the reference validates this module's arguments under, when it validates them at
    /// all, and so the name its refusal carries.
    ///
    /// Measured on ansible-core 2.19.12: `shell` is executed by the `command` module, so an
    /// argument neither module has is refused as `ansible.legacy.command` under both names and
    /// against one list. `raw` never reaches a module at all - its action plugin hands the line
    /// to the shell - so it takes any argument without looking at it, and a run that refused one
    /// would refuse a playbook the reference runs.
    pub validated_as: Option<&'static str>,
}

pub(super) const fn honoured(name: &'static str) -> ModuleArg {
    ModuleArg {
        name,
        status: ArgStatus::Honoured,
    }
}
