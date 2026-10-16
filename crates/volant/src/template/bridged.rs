// SPDX-License-Identifier: GPL-3.0-or-later
//! The filters, tests and lookups the engine has no native implementation of, run by
//! ansible-core in the controller's Python helper ([`Bridge`]).
//!
//! What crosses is data the engine has already rendered: the helper never templates it again.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, OnceLock};

use minijinja::value::{Kwargs, Rest, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State, Value};
use serde_json::Map;

use crate::bridge::{Bridge, BridgeError, PluginKind};

/// Names never sent to the helper, with why. Vault secrets come in a later release;
/// `inventory_hostnames` and `varnames` read the run's inventory and variables, which the helper
/// does not have.
const NOT_SENT: &[(PluginKind, &str, &str)] = &[
    (PluginKind::Filter, "vault", VAULT),
    (PluginKind::Filter, "unvault", VAULT),
    (PluginKind::Test, "vault_encrypted", VAULT),
    (PluginKind::Test, "vaulted_file", VAULT),
    (PluginKind::Lookup, "unvault", VAULT),
    (PluginKind::Lookup, "inventory_hostnames", RUN_DATA),
    (PluginKind::Lookup, "varnames", RUN_DATA),
];
const VAULT: &str = "vault secrets are not supported yet";
const RUN_DATA: &str =
    "reads the run's inventory and variables, which the Python helper does not have";

/// Why `name` is never sent to the helper, when it is not.
pub fn not_sent_reason(kind: PluginKind, name: &str) -> Option<&'static str> {
    NOT_SENT
        .iter()
        .find(|(k, n, _)| *k == kind && *n == short(name))
        .map(|(_, _, why)| *why)
}

/// Whether a template's `kind` named `name` runs in the Python helper: the engine has no native
/// implementation of it, and it is not one of the names the helper is never sent.
pub fn bridged_name(kind: PluginKind, name: &str) -> bool {
    static NATIVE: LazyLock<Environment<'static>> =
        LazyLock::new(|| super::environment(PathBuf::from("."), None));
    sendable_name(kind, name)
        && match kind {
            PluginKind::Lookup => !super::lookups::supported(short(name)),
            PluginKind::Filter | PluginKind::Test => !knows(&NATIVE, kind, name),
        }
}

/// Whether `name` may be sent to the helper at all.
pub(super) fn sendable_name(kind: PluginKind, name: &str) -> bool {
    not_sent_reason(kind, name).is_none()
}

/// `name` without the `ansible.builtin.` or `ansible.legacy.` the reference accepts on its own.
pub(super) fn short(name: &str) -> &str {
    name.strip_prefix("ansible.builtin.")
        .or_else(|| name.strip_prefix("ansible.legacy."))
        .unwrap_or(name)
}

/// Whether `text` reads as a plugin name, `word` or `word.word...`. Only such a string is ever
/// put into a probe expression ([`knows`]).
pub(super) fn is_name(text: &str) -> bool {
    text.split('.').all(|part| {
        part.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// A filter or test name as minijinja looks it up: `a | b . c` calls `b.c`.
pub(super) fn normalized(name: &str) -> String {
    name.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

/// Whether `env` has a filter or test of that name. minijinja has no public getter, so this
/// asks what it answers when a template calls it on an undefined value.
pub(super) fn knows(env: &Environment<'_>, kind: PluginKind, name: &str) -> bool {
    let probe = match kind {
        PluginKind::Test => format!("x is {name}"),
        PluginKind::Filter | PluginKind::Lookup => format!("x | {name}"),
    };
    let failure = match env.compile_expression(&probe) {
        Ok(expr) => expr.eval(()).err(),
        Err(err) => Some(err),
    };
    !failure.is_some_and(|e| matches!(e.kind(), ErrorKind::UnknownFilter | ErrorKind::UnknownTest))
}

/// Registers `name` in `env` as a call to the helper.
///
/// An undefined argument is not sent, as `_invoke_plugin` sends none to a plugin: once the name
/// is not known to be missing, a filter's result is undefined, a test fails as an undefined read.
pub(super) fn register(
    env: &mut Environment<'static>,
    bridge: &Arc<Bridge>,
    kind: PluginKind,
    name: &str,
) {
    let bridge = Arc::clone(bridge);
    let owned = name.to_string();
    let resolved = Arc::new(OnceLock::new());
    let call = move |_: &State, Rest(args): Rest<Value>| -> Result<Value, Error> {
        if super::any_undefined(&args) {
            // The name is resolved first, as ansible-core resolves it: a filter it does not
            // know fails even on an undefined value.
            // Only an answer that it does not exist fails here: with no helper to ask, the value
            // stays undefined, so `nope | x | default(...)` still answers.
            if let Err(err @ BridgeError::NotFound(_)) =
                resolved.get_or_init(|| bridge.resolve(kind, &owned))
            {
                return Err(failed(kind, &owned, err.clone()));
            }
            return match kind {
                PluginKind::Filter => Ok(Value::UNDEFINED),
                PluginKind::Test | PluginKind::Lookup => {
                    Err(Error::from(ErrorKind::UndefinedError))
                }
            };
        }
        let (positional, keywords) = match args.split_last() {
            Some((last, rest)) if last.is_kwargs() => (rest, keyword_values(last)?),
            _ => (&args[..], Vec::new()),
        };
        let (args, kwargs) = sendable(kind, &owned, positional, keywords)?;
        bridge
            .call(kind, &owned, &args, &kwargs, None)
            .map(|value| Value::from_serialize(&value))
            .map_err(|err| failed(kind, &owned, err))
    };
    match kind {
        PluginKind::Filter => env.add_filter(name.to_string(), call),
        PluginKind::Test => env.add_test(name.to_string(), call),
        PluginKind::Lookup => unreachable!("a lookup is a call to `lookup`, never registered"),
    }
}

/// `lookup(name, terms..., kwargs)` in the helper, with the variables ansible-core's own lookups
/// read off their templar. `wantlist` and `errors` travel with the call and the plugin shapes
/// its own answer. What a lookup returns is data, as `lookup('file')`'s is.
pub(super) fn lookup(
    state: &State,
    bridge: &Bridge,
    name: &str,
    terms: &[Value],
    kwargs: &Kwargs,
) -> Result<Value, Error> {
    let kind = PluginKind::Lookup;
    let mut keywords = Vec::new();
    for key in kwargs.args() {
        keywords.push((key.to_string(), kwargs.get::<Value>(key)?));
    }
    let (args, kwargs) = sendable(kind, name, terms, keywords)?;
    let mut vars = Map::new();
    for key in [
        "ansible_search_path",
        "role_path",
        "playbook_dir",
        "inventory_hostname",
        "group_names",
        "omit",
    ] {
        if let Some(value) = state.lookup(key).filter(|v| !v.is_undefined()) {
            vars.insert(key.to_string(), to_json(kind, name, &value)?);
        }
    }
    super::lookups::taint(state);
    bridge
        .call(kind, name, &args, &kwargs, Some(&vars))
        .map(|value| Value::from_serialize(&value))
        .map_err(|err| failed(kind, name, err))
}

/// The keyword arguments of a call, by name.
fn keyword_values(kwargs: &Value) -> Result<Vec<(String, Value)>, Error> {
    let mut out = Vec::new();
    for key in kwargs.try_iter()? {
        let value = kwargs.get_item(&key)?;
        out.push((key.to_string(), value));
    }
    Ok(out)
}

/// The arguments as JSON. An undefined value inside one is an undefined read here, where the
/// reference raises once the plugin reads it; a macro, `loop`, `caller` or function is refused
/// by name, as nothing in JSON carries it.
fn sendable(
    kind: PluginKind,
    name: &str,
    positional: &[Value],
    keywords: Vec<(String, Value)>,
) -> Result<(Vec<serde_json::Value>, Map<String, serde_json::Value>), Error> {
    let all = || positional.iter().chain(keywords.iter().map(|(_, v)| v));
    if all().any(super::holds_undefined) {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    if all().any(jinja_object) {
        return Err(raise(format!(
            "{} '{name}' was given a Jinja object, which cannot be sent to the Python helper",
            kind.as_str()
        )));
    }
    let args = positional
        .iter()
        .map(|v| to_json(kind, name, v))
        .collect::<Result<_, _>>()?;
    let mut kwargs = Map::new();
    for (key, value) in keywords {
        kwargs.insert(key, to_json(kind, name, &value)?);
    }
    Ok((args, kwargs))
}

fn to_json(kind: PluginKind, name: &str, value: &Value) -> Result<serde_json::Value, Error> {
    serde_json::to_value(value).map_err(|err| {
        raise(format!(
            "{} '{name}' was given a value that cannot be sent to the Python helper: {err}",
            kind.as_str()
        ))
    })
}

/// Whether `value` is, or holds, something only Jinja has: a macro or `caller`, `loop`, a
/// function.
fn jinja_object(value: &Value) -> bool {
    if matches!(value.kind(), ValueKind::Plain | ValueKind::Invalid) {
        return true;
    }
    // Both are maps to minijinja, and would be sent as their attributes.
    if let Some(object) = value.as_object() {
        let type_name = object.type_name();
        if type_name.ends_with("::Macro") || type_name.ends_with("::Loop") {
            return true;
        }
    }
    match value.kind() {
        ValueKind::Seq | ValueKind::Iterable => value
            .try_iter()
            .is_ok_and(|mut items| items.any(|item| jinja_object(&item))),
        ValueKind::Map => value.try_iter().is_ok_and(|mut keys| {
            keys.any(|key| value.get_item(&key).is_ok_and(|item| jinja_object(&item)))
        }),
        _ => false,
    }
}

/// A bridged call's failure as the template error says it. A plugin's own failure is
/// ansible-core's sentence; a filter or test ansible-core does not know either fails as an
/// unknown name does here.
fn failed(kind: PluginKind, name: &str, err: BridgeError) -> Error {
    let what = kind.as_str();
    match (err, kind) {
        (BridgeError::NotFound(_), PluginKind::Filter) => Error::new(
            ErrorKind::UnknownFilter,
            format!("filter {name} is unknown"),
        ),
        (BridgeError::NotFound(_), PluginKind::Test) => {
            Error::new(ErrorKind::UnknownTest, format!("test {name} is unknown"))
        }
        (BridgeError::Plugin(text) | BridgeError::NotFound(text), _) => raise(text),
        (BridgeError::Start(why), _) => raise(format!(
            "{what} '{name}' needs the Python helper, which could not start: {why}"
        )),
        (BridgeError::Helper(why), _) => raise(format!(
            "{what} '{name}' needs the Python helper, which failed: {why}"
        )),
    }
}

/// A message that is the whole of what the template error says, see [`said`].
#[derive(Debug)]
struct Said(String);

impl fmt::Display for Said {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Said {}

fn raise(text: String) -> Error {
    Error::new(ErrorKind::InvalidOperation, text.clone()).with_source(Said(text))
}

/// The message a bridged call raised, without minijinja's kind and location around it.
pub(super) fn said(err: &Error) -> Option<&str> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(err) = source {
        if let Some(Said(text)) = err.downcast_ref::<Said>() {
            return Some(text);
        }
        source = err.source();
    }
    None
}

/// Whether a template error is a plugin's own failure, `The <kind> plugin '<name>' failed: …`
/// or `The lookup plugin '<name>' was not found.`, which the reference chains behind the value
/// it was resolving.
pub(crate) fn is_plugin_failure(message: &str) -> bool {
    [
        "The filter plugin '",
        "The test plugin '",
        "The lookup plugin '",
    ]
    .iter()
    .any(|head| message.starts_with(head))
}
