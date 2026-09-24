// SPDX-License-Identifier: GPL-3.0-or-later
//! Jinja2 templating the way ansible-core 2.19 does it: strict about undefined variables, and a
//! template that is one expression yields that expression's value, not its text.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use minijinja::functions::Function;
use minijinja::value::{Enumerator, FunctionArgs, FunctionResult, Object, Rest, ValueKind};
use minijinja::{Environment, ErrorKind, State, UndefinedBehavior};
use serde_json::{Map, Value};

mod file;
mod filters;
pub(crate) use filters::python_json;
mod lookups;
mod tests;
mod yaml_dump;

pub use file::FileRender;

/// Prefix of every error raised because a variable had no value, the one template failure
/// Ansible treats as recoverable in places such as `vars_files`.
const UNDEFINED: &str = "The task includes an option with an undefined variable.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError(pub String);

impl TemplateError {
    /// Whether the render failed on a variable that had no value, as opposed to a syntax error,
    /// an unknown filter or an illegal operation.
    pub fn is_undefined(&self) -> bool {
        self.0.starts_with(UNDEFINED)
    }
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TemplateError {}

/// Set while a render pass reads a name whose value came from a managed host rather than from
/// the playbook. A result built from such a name is data, so the extra passes below do not run
/// on it and an expression that arrived in a command's output is never evaluated here.
pub(crate) type Tainted = Arc<AtomicBool>;

/// The name `Context` answers with the render's taint sink. Two colons cannot appear in a Jinja
/// identifier, so no playbook can write it as a variable or shadow it; `lookup('vars', ...)`
/// reaches it, and hands back an object whose only method marks the render as data.
pub(crate) const TAINT_KEY: &str = "volant::tainted";

/// The name `Context` answers with itself, for `lookup('template')`, which renders a file
/// against the same variables and the same trust as the render that called it. Unreachable from
/// a playbook for the same reason as `TAINT_KEY`; through `lookup('vars', ...)` it hands back
/// the root context, whose every read goes through `get_value` and taints as a bare read does.
pub(crate) const CONTEXT_KEY: &str = "volant::context";

#[derive(Debug)]
pub(crate) struct TaintSink(pub(crate) Tainted);

impl TaintSink {
    pub(crate) fn taint(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Object for TaintSink {}

/// What a template renders against: one host's own variables, and — on the path where a host
/// runs a task — the whole inventory's view behind `hostvars`.
///
/// The view is shared rather than merged into the map, because merging it copied every host's
/// variables into every host's context for every task, which is a cost quadratic in the size of
/// the inventory for something most tasks never read. Anything holding only a map converts with
/// `From`, so a caller with no inventory in hand passes `&Map` and gets no `hostvars`.
#[derive(Clone, Copy)]
pub struct Vars<'a> {
    pub map: &'a Map<String, Value>,
    pub hostvars: Option<&'a Arc<Map<String, Value>>>,
    /// The values the whole inventory shares — `groups` and the play's host lists — read out of
    /// here rather than copied into every host's map for every task. Consulted *before* `map`,
    /// because these used to be written into it last and last is what wins. A name written into
    /// the map later still wins, because `HostVars::insert` takes it out of here first.
    pub shared: Option<&'a Arc<Map<String, Value>>>,
    /// Names in `map` whose value came from a managed host. Reading one of them during a render
    /// makes the result data: see `render_in`.
    pub untrusted: Option<&'a BTreeSet<String>>,
    /// Hosts that have at least one such name, for the `hostvars[other]` path. Coarser than the
    /// reference, which tags each string: reading any name of such a host taints the render.
    pub untrusted_hosts: Option<&'a BTreeSet<String>>,
}

impl<'a> From<&'a Map<String, Value>> for Vars<'a> {
    fn from(map: &'a Map<String, Value>) -> Self {
        Vars {
            map,
            hostvars: None,
            shared: None,
            untrusted: None,
            untrusted_hosts: None,
        }
    }
}

/// Converted values, kept for as long as the object holding them. Converting a name when it is
/// asked for rather than converting the whole map up front only pays off if asking twice costs
/// once: a template reading `groups` inside a loop asks for it on every iteration, and each of
/// those conversions is the size of the inventory. Nothing behind a memo changes while it lives -
/// the map is cloned into the context and the shared view is replaced whole, never mutated - so
/// there is nothing to invalidate.
#[derive(Debug, Default)]
struct Memo(Mutex<HashMap<String, minijinja::Value>>);

impl Memo {
    fn get(
        &self,
        key: &str,
        convert: impl FnOnce() -> Option<minijinja::Value>,
    ) -> Option<minijinja::Value> {
        let mut cache = self.0.lock().expect("template memo");
        if let Some(value) = cache.get(key) {
            return Some(value.clone());
        }
        let value = convert()?;
        cache.insert(key.to_string(), value.clone());
        Some(value)
    }
}

/// The root of a render: the host's variables, each converted when it is asked for rather than
/// all of them up front, and `hostvars` as an object that hands out one host's view on demand.
#[derive(Debug)]
struct Context {
    vars: Map<String, Value>,
    hostvars: Option<Arc<Map<String, Value>>>,
    shared: Option<Arc<Map<String, Value>>>,
    untrusted: BTreeSet<String>,
    untrusted_hosts: BTreeSet<String>,
    tainted: Tainted,
    memo: Memo,
}

impl Object for Context {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let key = key.as_str()?;
        if key == TAINT_KEY {
            return Some(minijinja::Value::from_object(TaintSink(Arc::clone(
                &self.tainted,
            ))));
        }
        if key == CONTEXT_KEY {
            return Some(minijinja::Value::from_dyn_object(Arc::clone(self)));
        }
        // Before the memo, never after: a second read of the same name is answered from the
        // cache and would otherwise leave the render looking clean.
        if self.untrusted.contains(key) {
            self.tainted.store(true, Ordering::Relaxed);
        }
        self.memo.get(key, || {
            // A map that carries a `hostvars` key of its own - a fixture, a recorded scope -
            // keeps being read from the map when no view came with it, so the two sources never
            // disagree about which one answers.
            if key == "hostvars"
                && let Some(hostvars) = &self.hostvars
            {
                return Some(minijinja::Value::from_object(Hostvars {
                    hosts: Arc::clone(hostvars),
                    untrusted_hosts: self.untrusted_hosts.clone(),
                    tainted: Arc::clone(&self.tainted),
                    memo: Memo::default(),
                }));
            }
            // Before the host's own map, never after: these are the inventory-wide values, and
            // they used to be merged into that map last, so they beat a fact of the same name.
            // A loop variable or a registered name that collides with one of them is not here
            // to be found: `HostVars::insert` drops it from the host's shared map as it writes.
            if let Some(shared) = &self.shared
                && let Some(value) = shared.get(key)
            {
                return Some(minijinja::Value::from_serialize(value));
            }
            self.vars.get(key).map(minijinja::Value::from_serialize)
        })
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let mut keys: Vec<minijinja::Value> = self
            .vars
            .keys()
            .map(|k| minijinja::Value::from(k.as_str()))
            .collect();
        if self.hostvars.is_some() && !self.vars.contains_key("hostvars") {
            keys.push(minijinja::Value::from("hostvars"));
        }
        for key in self.shared.iter().flat_map(|s| s.keys()) {
            if !self.vars.contains_key(key) {
                keys.push(minijinja::Value::from(key.as_str()));
            }
        }
        Enumerator::Values(keys)
    }
}

/// `hostvars`: a map whose values are converted one host at a time. Iterating it still costs
/// the whole inventory, which is what a `hostvars | dict2items` asks for; reading one host
/// costs one host.
#[derive(Debug)]
struct Hostvars {
    hosts: Arc<Map<String, Value>>,
    /// Hosts holding at least one name that came from a managed host. Reading any name of such
    /// a host taints the render, which is coarser than the reference's per-string tag.
    untrusted_hosts: BTreeSet<String>,
    tainted: Tainted,
    memo: Memo,
}

impl Object for Hostvars {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let key = key.as_str()?;
        // Before the memo, for the same reason as in `Context`.
        if self.untrusted_hosts.contains(key) {
            self.tainted.store(true, Ordering::Relaxed);
        }
        self.memo.get(key, || {
            self.hosts.get(key).map(minijinja::Value::from_serialize)
        })
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Values(
            self.hosts
                .keys()
                .map(|k| minijinja::Value::from(k.as_str()))
                .collect(),
        )
    }
}

fn context_of(vars: Vars<'_>) -> (minijinja::Value, Tainted) {
    let tainted: Tainted = Arc::new(AtomicBool::new(false));
    let ctx = minijinja::Value::from_object(Context {
        vars: vars.map.clone(),
        hostvars: vars.hostvars.cloned(),
        shared: vars.shared.cloned(),
        untrusted: vars.untrusted.cloned().unwrap_or_default(),
        untrusted_hosts: vars.untrusted_hosts.cloned().unwrap_or_default(),
        tainted: Arc::clone(&tainted),
        memo: Memo::default(),
    });
    (ctx, tainted)
}

pub struct Templar {
    env: Environment<'static>,
}

impl Templar {
    /// `base_dir` anchors relative paths in lookups such as `lookup('file', ...)`.
    pub fn new(base_dir: PathBuf) -> Self {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        // Strict mode refuses to print an undefined value, not a list or mapping holding one,
        // which minijinja prints as `[undefined]`. Measured on ansible-core 2.19.12,
        // `x {{ [nope] }}` is an undefined read.
        env.set_formatter(|out, state, value| {
            if !value.is_undefined() && holds_undefined(value) {
                return Err(minijinja::Error::from(ErrorKind::UndefinedError));
            }
            minijinja::escape_formatter(out, state, value)
        });
        filters::register(&mut env, base_dir);
        Self { env }
    }

    pub fn is_template(text: &str) -> bool {
        text.contains("{{") || text.contains("{%")
    }

    /// Renders a string. Exactly one `{{ expression }}` gives the expression's value with its
    /// type; text around or between expressions gives a string.
    pub fn render<'a>(
        &self,
        text: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<Value, TemplateError> {
        let (ctx, tainted) = context_of(vars.into());
        self.render_in(text, &ctx, &tainted)
    }

    fn render_in(
        &self,
        text: &str,
        ctx: &minijinja::Value,
        tainted: &Tainted,
    ) -> Result<Value, TemplateError> {
        // Per string, not per context: `render_value_in` walks a whole structure through one
        // context, and a tainted field must not make the next field data too.
        tainted.store(false, Ordering::Relaxed);
        let mut value = self.render_once(text, ctx)?;
        // A variable can hold a template of its own, so Ansible renders a result again while it
        // still carries a marker. Three further passes: a chain longer than that is a loop, and
        // an unchanged result ends it earlier.
        //
        // Not when the pass read a value that came from a managed host, or a file read at run
        // time: such a result is data, and rendering it again is how a remote string gets to
        // run code here.
        for _ in 0..3 {
            if tainted.load(Ordering::Relaxed) {
                break;
            }
            let Value::String(text) = &value else { break };
            if !Self::is_template(text) {
                break;
            }
            let next = self.render_once(text, ctx)?;
            if next == value {
                break;
            }
            value = next;
        }
        Ok(value)
    }

    fn render_once(&self, text: &str, ctx: &minijinja::Value) -> Result<Value, TemplateError> {
        if !Self::is_template(text) {
            return Ok(Value::String(text.to_string()));
        }
        if let Some(expr) = single_expression(text) {
            return self.evaluate_in(expr, ctx);
        }
        self.env
            .render_str(text, ctx)
            .map(Value::String)
            .map_err(convert_error)
    }

    /// Templates every string inside a value. Mapping keys are left alone.
    pub fn render_value<'a>(
        &self,
        value: &Value,
        vars: impl Into<Vars<'a>>,
    ) -> Result<Value, TemplateError> {
        Ok(self.render_value_tainted(value, vars)?.0)
    }

    /// `render_value`, saying as well whether what came out is data: true when any string of it
    /// was rendered by a pass that read a name from a managed host, or read a file at run time.
    /// The loop path asks, because the items of a loop built from a registered list are data and
    /// must not be rendered again once they are bound to the loop variable.
    pub fn render_value_tainted<'a>(
        &self,
        value: &Value,
        vars: impl Into<Vars<'a>>,
    ) -> Result<(Value, bool), TemplateError> {
        let (ctx, tainted) = context_of(vars.into());
        let mut any = false;
        let out = self.render_value_in(value, &ctx, &tainted, &mut any)?;
        Ok((out, any))
    }

    /// `render_value` over a map, keeping the answer **per key**: the names whose own render
    /// read a managed host come back beside the rendered map.
    ///
    /// A task's arguments are mostly data, and data is what a rendered value should be. A few
    /// of them are not: an argument the engine reads back as source text - the name a
    /// `debug: var:` compiles - is engine input, and the only moment its provenance exists is
    /// the render that produced it. One answer for the whole map would refuse an argument the
    /// playbook wrote because a sibling argument came from a host.
    pub fn render_map_tainted<'a>(
        &self,
        map: &Map<String, Value>,
        vars: impl Into<Vars<'a>>,
    ) -> Result<(Map<String, Value>, BTreeSet<String>), TemplateError> {
        let (ctx, tainted) = context_of(vars.into());
        let mut out = Map::new();
        let mut untrusted = BTreeSet::new();
        for (key, value) in map {
            let mut any = false;
            out.insert(
                key.clone(),
                self.render_value_in(value, &ctx, &tainted, &mut any)?,
            );
            if any {
                untrusted.insert(key.clone());
            }
        }
        Ok((out, untrusted))
    }

    fn render_value_in(
        &self,
        value: &Value,
        ctx: &minijinja::Value,
        tainted: &Tainted,
        any: &mut bool,
    ) -> Result<Value, TemplateError> {
        Ok(match value {
            Value::String(s) => {
                let out = self.render_in(s, ctx, tainted)?;
                *any |= tainted.load(Ordering::Relaxed);
                out
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|v| self.render_value_in(v, ctx, tainted, any))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(map) => {
                let mut out = Map::new();
                for (k, v) in map {
                    out.insert(k.clone(), self.render_value_in(v, ctx, tainted, any)?);
                }
                Value::Object(out)
            }
            other => other.clone(),
        })
    }

    /// Evaluates one expression and returns its value with its type.
    pub fn evaluate<'a>(
        &self,
        expr: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<Value, TemplateError> {
        // `evaluate_in` takes one pass and no more, so a result built from a name that came from
        // a managed host is already handed back as it stands: there is nothing here to stop.
        self.evaluate_in(expr, &context_of(vars.into()).0)
    }

    fn evaluate_in(&self, expr: &str, ctx: &minijinja::Value) -> Result<Value, TemplateError> {
        let compiled = self.env.compile_expression(expr).map_err(convert_error)?;
        let value = compiled.eval(ctx).map_err(convert_error)?;
        // Strict mode only raises on operations that force an undefined value (printing,
        // comparing, ...); an attribute lookup that never gets used stays a lazy Undefined, alone
        // or inside a list or mapping, that serde_json would otherwise turn into `null`. Force
        // the same error here.
        if holds_undefined(&value) {
            return Err(convert_error(minijinja::Error::from(
                ErrorKind::UndefinedError,
            )));
        }
        serde_json::to_value(&value)
            .map_err(|e| TemplateError(format!("cannot convert template result: {e}")))
    }

    /// Templates the string values of a variable map against the map itself, a few passes, until
    /// nothing changes. Best effort: a value that fails to render is left as written, so the
    /// error surfaces where the value is used, as it does in Ansible.
    ///
    /// A name that came from a managed host is left exactly as it arrived. This is the other
    /// half of the barrier `render_in` holds: the merged map a task renders against carries the
    /// facts, so a registered value whose text looks like an expression would be evaluated here,
    /// one task before anything even reads it.
    ///
    /// The names that are data once it is done travel out with the map: the ones it was given
    /// plus the ones a pass turned into data by rendering them from one of those.
    ///
    /// The second half is the part a caller cannot work out for itself. A task's own `vars:` are
    /// resolved here and read nowhere else, so a name the playbook wrote over a registered value
    /// has no entry anywhere to be looked up by: unless this set travels out with the map, the
    /// arguments rendered against that map read the payload as author content and evaluate it.
    pub fn resolve_vars_tainted<'a>(
        &self,
        vars: impl Into<Vars<'a>>,
    ) -> (Map<String, Value>, BTreeSet<String>) {
        let vars = vars.into();
        let given = || vars.untrusted.cloned().unwrap_or_default();
        // Most maps hold no template at all: walking them once is far cheaper than the two
        // clones a pass costs.
        if !vars.map.values().any(holds_template) {
            return (vars.map.clone(), given());
        }
        // The names the store knows about, and the ones a pass turns into data as it goes: a
        // value rendered from a managed host's name, or from a file read at run time, is data
        // from that pass on and the passes after this one leave it alone.
        let mut untrusted = given();
        let mut current = vars.map.clone();
        for _ in 0..5 {
            // One context for the whole pass: every value of the map renders against the same
            // map, so building it per value paid for the same conversion once per variable.
            let (ctx, tainted) = context_of(Vars {
                map: &current,
                hostvars: vars.hostvars,
                shared: vars.shared,
                untrusted: Some(&untrusted),
                untrusted_hosts: vars.untrusted_hosts,
            });
            let mut next = current.clone();
            let mut changed = false;
            let mut soiled = Vec::new();
            for (k, v) in &current {
                if untrusted.contains(k) {
                    continue;
                }
                let mut any = false;
                let rendered = self.render_value_lenient(v, &ctx, &tainted, &mut any);
                if any {
                    soiled.push(k.clone());
                }
                if rendered != *v {
                    changed = true;
                    next.insert(k.clone(), rendered);
                }
            }
            untrusted.extend(soiled);
            current = next;
            if !changed {
                break;
            }
        }
        (current, untrusted)
    }

    fn render_value_lenient(
        &self,
        value: &Value,
        ctx: &minijinja::Value,
        tainted: &Tainted,
        any: &mut bool,
    ) -> Value {
        match value {
            Value::String(s) if Self::is_template(s) => {
                let out = self
                    .render_in(s, ctx, tainted)
                    .unwrap_or_else(|_| value.clone());
                *any |= tainted.load(Ordering::Relaxed);
                out
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|v| self.render_value_lenient(v, ctx, tainted, any))
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.render_value_lenient(v, ctx, tainted, any)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// A `when` clause. Ansible accepts `{{ }}` around it with a warning; the result must be a
    /// boolean, anything else is an error in the reference release.
    ///
    /// A delimited condition that renders to a string is evaluated again as a condition, the
    /// way the reference treats a string an author wrote (`healthy: "true"`, `cond: "1 == 1"`).
    /// Never when the first pass read a value from a managed host or a lookup's text: that
    /// string is data, and compiling it is how a host would run code here.
    pub fn condition<'a>(
        &self,
        expr: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<bool, TemplateError> {
        let (ctx, tainted) = context_of(vars.into());
        let delimited = single_expression(expr);
        let mut value = self.evaluate_in(delimited.unwrap_or(expr.trim()), &ctx)?;
        if delimited.is_some()
            && let Value::String(text) = &value
        {
            if tainted.load(Ordering::Relaxed) {
                return Err(TemplateError(
                    "Encountered untrusted template or expression.".into(),
                ));
            }
            value = self.evaluate_in(single_expression(text).unwrap_or(text.trim()), &ctx)?;
        }
        match value {
            Value::Bool(b) => Ok(b),
            other => Err(TemplateError(format!(
                "Conditional result was {other} of type {}, which evaluates to {}. Conditionals must have a boolean result.",
                type_name(&other),
                if truthy(&other) { "True" } else { "False" }
            ))),
        }
    }
}

/// Whether any string inside a value still carries a template marker.
fn holds_template(value: &Value) -> bool {
    match value {
        Value::String(s) => Templar::is_template(s),
        Value::Array(items) => items.iter().any(holds_template),
        Value::Object(map) => map.values().any(holds_template),
        _ => false,
    }
}

/// `{{ expr }}` and nothing else: no text around, no second expression, no statement.
fn single_expression(text: &str) -> Option<&str> {
    let t = text.trim();
    let inner = t.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") || inner.contains("{%") {
        return None;
    }
    Some(inner.trim())
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Registers a filter under `name` and again under `ansible.builtin.<name>`: every filter and
/// test ansible-core ships lives in that collection, and a role is free to spell it out in full.
/// Measured (A9): minijinja's parser accepts a dotted name after `|` or `is` as a single filter
/// or test name — `{{ "x" | ansible.builtin.default("y") }}` fails by `unknown filter: filter
/// ansible.builtin.default is unknown`, not by a syntax error — so registering the qualified
/// string is all a name needs. `ansible.utils.ipwrap` is registered on its own, never through
/// this: the reference never exposes it under `ansible.builtin`.
///
/// Measured on ansible-core 2.19.12, a filter given an undefined value does not run: the result
/// is undefined too, so `nope | dict2items` fails as an undefined read and
/// `nope | dict2items | default('x')` is `x`. The filter is wrapped here to do the same, except
/// the few that exist to look at an undefined value.
pub(crate) fn add_filter<F, Rv, Args>(env: &mut Environment<'static>, name: &str, f: F)
where
    F: Function<Rv, Args>,
    Rv: FunctionResult,
    Args: for<'a> FunctionArgs<'a>,
{
    let filter = undefined_through(name, f);
    env.add_filter(format!("ansible.builtin.{name}"), filter.clone());
    env.add_filter(name.to_string(), filter);
}

/// [`add_filter`] for a filter that turns a container into text or a number (`join`, `string`,
/// `quote`, `sum`): an undefined value anywhere inside an argument fails as an undefined read.
/// Measured on ansible-core 2.19.12, `[1, nope] | join(',')` fails where `[nope] | length` and
/// `[1, nope] | first` answer `1`, so the filters that only count or pick keep [`add_filter`].
pub(crate) fn add_text_filter<F, Rv, Args>(env: &mut Environment<'static>, name: &str, f: F)
where
    F: Function<Rv, Args>,
    Rv: FunctionResult,
    Args: for<'a> FunctionArgs<'a>,
{
    let f = undefined_through(name, f);
    let filter = move |state: &State, args: Rest<minijinja::Value>| {
        if args.iter().any(|v| !v.is_undefined() && holds_undefined(v)) {
            return Err(minijinja::Error::from(ErrorKind::UndefinedError));
        }
        f(state, args)
    };
    env.add_filter(format!("ansible.builtin.{name}"), filter.clone());
    env.add_filter(name.to_string(), filter);
}

/// `f`, answering undefined when an argument it is given is undefined; see [`add_filter`].
/// Measured on ansible-core 2.19.12: any argument counts, keyword ones included
/// (`'%s' | format(nope | lower)`, `regex_replace('a', 'b', ignorecase=nope | bool)` are
/// undefined reads), but only the argument itself, not what it holds (`[nope | lower] | length`
/// is 1). `ternary` only looks at its input (`true | ternary('a', nope)` is `a`); `default`,
/// `d`, `mandatory` and `type_debug` look at nothing.
pub(crate) fn undefined_through<F, Rv, Args>(
    name: &str,
    f: F,
) -> impl Fn(&State, Rest<minijinja::Value>) -> Result<minijinja::Value, minijinja::Error>
+ Clone
+ Send
+ Sync
+ 'static
where
    F: Function<Rv, Args>,
    Rv: FunctionResult,
    Args: for<'a> FunctionArgs<'a>,
{
    let f = minijinja::Value::from_function(f);
    let checked = match name {
        "default" | "d" | "mandatory" | "type_debug" => 0,
        "ternary" => 1,
        _ => usize::MAX,
    };
    move |state: &State, args: Rest<minijinja::Value>| {
        if any_undefined(&args[..checked.min(args.len())]) {
            return Ok(minijinja::Value::UNDEFINED);
        }
        f.call(state, &args)
    }
}

/// Whether one of these arguments, or one keyword argument among them, is undefined.
fn any_undefined(args: &[minijinja::Value]) -> bool {
    args.iter().any(|v| {
        v.is_undefined()
            || (v.is_kwargs()
                && v.try_iter().is_ok_and(|mut keys| {
                    keys.any(|k| v.get_item(&k).is_ok_and(|x| x.is_undefined()))
                }))
    })
}

/// Whether a value is undefined or holds an undefined value in one of its lists or mappings.
/// minijinja keeps an undefined value inside a literal (`[nope]`, `{'k': nope | int}`) or a
/// concatenation (`items + [nope]`, a lazy sequence) and serialises it as `null`; the reference fails on it as an undefined read (measured on
/// ansible-core 2.19.12), so every place a value leaves the template engine asks this first.
pub(crate) fn holds_undefined(v: &minijinja::Value) -> bool {
    match v.kind() {
        ValueKind::Undefined => true,
        ValueKind::Seq | ValueKind::Iterable => v
            .try_iter()
            .is_ok_and(|mut it| it.any(|x| holds_undefined(&x))),
        ValueKind::Map => v
            .try_iter()
            .is_ok_and(|mut keys| keys.any(|k| v.get_item(&k).is_ok_and(|x| holds_undefined(&x)))),
        _ => false,
    }
}

/// The test equivalent of [`add_filter`]. A test given an undefined argument fails as an
/// undefined read (`nope is none`, measured), except `defined` and `undefined`.
pub(crate) fn add_test<F, Rv, Args>(env: &mut Environment<'static>, name: &str, f: F)
where
    F: Function<Rv, Args>,
    Rv: FunctionResult,
    Args: for<'a> FunctionArgs<'a>,
{
    let f = minijinja::Value::from_function(f);
    let sees_undefined = ["defined", "undefined"].contains(&name);
    let test = move |state: &State, args: Rest<minijinja::Value>| {
        if !sees_undefined && any_undefined(&args) {
            return Err(minijinja::Error::from(ErrorKind::UndefinedError));
        }
        f.call(state, &args)
    };
    env.add_test(format!("ansible.builtin.{name}"), test.clone());
    env.add_test(name.to_string(), test);
}

/// Python truthiness, for messages and for filters that need it.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn convert_error(err: minijinja::Error) -> TemplateError {
    let text = err.to_string();
    if err.kind() == ErrorKind::UndefinedError {
        return TemplateError(format!("{UNDEFINED} The error was: {text}"));
    }
    TemplateError(text)
}

#[cfg(test)]
mod unit {
    use super::*;
    use serde_json::json;

    fn vars(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn a_single_expression_keeps_its_type_and_text_renders_to_a_string() {
        let t = Templar::new(std::env::temp_dir());
        assert_eq!(t.render("{{ 1 + 1 }}", &Map::new()).unwrap(), json!(2));
        assert_eq!(
            t.render("{{ flag }}", &vars(json!({"flag": true})))
                .unwrap(),
            json!(true)
        );
        assert_eq!(
            t.render("{{ items }}", &vars(json!({"items": [1, 2]})))
                .unwrap(),
            json!([1, 2])
        );
        assert_eq!(
            t.render("n={{ 1 + 1 }}", &Map::new()).unwrap(),
            json!("n=2")
        );
        assert_eq!(
            t.render("{{ a }}{{ b }}", &vars(json!({"a": 1, "b": 2})))
                .unwrap(),
            json!("12")
        );
        assert_eq!(t.render("plain", &Map::new()).unwrap(), json!("plain"));
    }

    #[test]
    fn undefined_variables_are_errors_with_ansible_wording() {
        let t = Templar::new(std::env::temp_dir());
        let err = t.render("{{ missing }}", &Map::new()).unwrap_err();
        assert!(
            err.0
                .starts_with("The task includes an option with an undefined variable."),
            "{err}"
        );
        assert!(
            t.render("{{ conf.missing }}", &vars(json!({"conf": {}})))
                .is_err()
        );
    }

    #[test]
    fn values_are_templated_recursively_but_keys_are_not() {
        let t = Templar::new(std::env::temp_dir());
        let v = t
            .render_value(
                &json!({"{{ k }}": ["{{ x }}", {"n": "{{ x + 1 }}"}], "lit": 3}),
                &vars(json!({"k": "key", "x": 1})),
            )
            .unwrap();
        assert_eq!(v, json!({"{{ k }}": [1, {"n": 2}], "lit": 3}));
    }

    #[test]
    fn resolve_vars_chains_references_and_leaves_broken_ones_alone() {
        let t = Templar::new(std::env::temp_dir());
        let resolved = t
            .resolve_vars_tainted(&vars(
                json!({"a": "{{ b }}", "b": "{{ c }}!", "c": "x", "bad": "{{ missing }}"}),
            ))
            .0;
        assert_eq!(resolved["a"], json!("x!"));
        assert_eq!(resolved["c"], json!("x"));
        assert_eq!(resolved["bad"], json!("{{ missing }}"));
    }

    /// The five shapes a playbook reads `hostvars` with. They are listed here as one test so
    /// the map and the shared object below can be held to the same five answers.
    fn hostvars_cases() -> [(&'static str, Value); 5] {
        [
            (
                "{{ hostvars | dict2items | map(attribute='key') | sort | join(',') }}",
                json!("a,b"),
            ),
            ("{{ hostvars | list | sort | join(',') }}", json!("a,b")),
            ("{{ hostvars['a'].y }}", json!(1)),
            ("{{ 'b' in hostvars }}", json!(true)),
            ("{{ hostvars | length }}", json!(2)),
        ]
    }

    fn hostvars_map() -> Map<String, Value> {
        vars(json!({"hostvars": {"a": {"y": 1}, "b": {"y": 2}}}))
    }

    #[test]
    fn hostvars_reads_the_five_ways_a_playbook_reads_it() {
        let t = Templar::new(std::env::temp_dir());
        let map = hostvars_map();
        for (text, want) in hostvars_cases() {
            assert_eq!(t.render(text, &map).unwrap(), want, "{text}");
        }
    }

    /// The same five, read off the shared view instead of a map that carries it. The two must
    /// answer alike, because this is the substitution the render path makes.
    #[test]
    fn the_shared_view_answers_the_five_the_same_way() {
        let t = Templar::new(std::env::temp_dir());
        let shared = Arc::new(vars(json!({"a": {"y": 1}, "b": {"y": 2}})));
        let empty = Map::new();
        for (text, want) in hostvars_cases() {
            let vars = Vars {
                map: &empty,
                hostvars: Some(&shared),
                shared: None,
                untrusted: None,
                untrusted_hosts: None,
            };
            assert_eq!(t.render(text, vars).unwrap(), want, "{text}");
        }
    }

    /// What the whole change rests on: a template asking for `hostvars` is handed the shared
    /// map itself, never a conversion of it. `ptr_eq` is the statement `Arc::strong_count`
    /// would only hint at - the same allocation, so no host's variables were copied to build
    /// the context.
    ///
    /// What would make this red: `context_of` serialising the view into the context, which is
    /// what the map used to carry and what cost a copy of every host's variables per task.
    ///
    /// The second read is the memo. Converting a name on demand instead of converting the whole
    /// map up front is only cheaper if the second ask is free, and a template reading an
    /// inventory-wide name inside a loop asks once per iteration. Dropping the memo makes the
    /// two reads two different objects and this red.
    #[test]
    fn the_context_hands_out_the_shared_view_itself_and_only_builds_it_once() {
        let shared = Arc::new(vars(json!({"a": {"y": 1}, "b": {"y": 2}})));
        let empty = Map::new();
        let (ctx, _) = context_of(Vars {
            map: &empty,
            hostvars: Some(&shared),
            shared: None,
            untrusted: None,
            untrusted_hosts: None,
        });
        let first = ctx
            .get_attr("hostvars")
            .expect("hostvars is in the context")
            .downcast_object::<Hostvars>()
            .expect("the shared object, not a copy of the map");
        assert!(Arc::ptr_eq(&first.hosts, &shared));
        let second = ctx
            .get_attr("hostvars")
            .unwrap()
            .downcast_object::<Hostvars>()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second), "the view is built once");
        drop(first);
        drop(second);
        drop(ctx);
        assert_eq!(
            Arc::strong_count(&shared),
            1,
            "and the context gives it back when it goes"
        );
    }

    /// The inventory-wide values are read out of the shared map and not out of the host's own,
    /// and they still beat a fact of the same name — which is the order they had when they were
    /// written into that map last.
    ///
    /// What would make this red: consulting the host's map before the shared one. A `set_fact`
    /// named `groups` would then win, and every template reading `groups` after it would read
    /// the fact instead of the inventory.
    #[test]
    fn a_fact_does_not_shadow_an_inventory_wide_value() {
        let t = Templar::new(std::env::temp_dir());
        let shared = Arc::new(vars(json!({"groups": {"web": ["h1"]}})));
        let fact = vars(json!({"groups": "a set_fact wrote this", "own": 1}));
        let vars = Vars {
            map: &fact,
            hostvars: None,
            shared: Some(&shared),
            untrusted: None,
            untrusted_hosts: None,
        };
        assert_eq!(
            t.render("{{ groups['web'] }}", vars).unwrap(),
            json!(["h1"])
        );
        // And the host's own names are still its own: the shared map answers for what it holds
        // and for nothing else.
        assert_eq!(t.render("{{ own }}", vars).unwrap(), json!(1));
    }

    /// A name both maps carry is listed once when the context is walked as a mapping. A
    /// `set_fact: groups=x` produces exactly that state: the fact is merged into the host's map
    /// and the inventory's `groups` is in the shared one.
    ///
    /// No playbook reaches this today. The root context is not a value a template can name -
    /// there is no `vars` magic variable to walk, and `lookup('vars', ...)` resolves one name
    /// at a time through `get_value` - so nothing renders it as a mapping. The guard is still
    /// the difference between a list of names and a list with a duplicate in it, and the first
    /// thing to name the root context would read `groups` twice and hand `| dict2items` two
    /// entries with the same key.
    ///
    /// What would make this red: dropping the `contains_key` guard from `enumerate`.
    #[test]
    fn a_name_both_maps_carry_is_listed_once() {
        let shared = Arc::new(vars(json!({"groups": {"web": ["h1"]}})));
        let fact = vars(json!({"groups": "a set_fact wrote this", "own": 1}));
        let (ctx, _) = context_of(Vars {
            map: &fact,
            hostvars: None,
            shared: Some(&shared),
            untrusted: None,
            untrusted_hosts: None,
        });
        let mut names: Vec<String> = ctx
            .try_iter()
            .expect("the root context walks as a mapping")
            .map(|name| name.to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["groups", "own"]);
    }

    /// The `loop:` of k3s-ansible's `prereq` task "If firewalld enabled, allow node CIDRs",
    /// copied as it stands. Measured on ansible-core 2.19.12, local connection, `h1` in the
    /// server group and `h2`, `h3` in the agent group, with `default_ipv4` among the facts of
    /// `h1` and `h3` only: `["192.0.2.1", "192.0.2.3"]`, and `["192.0.2.3"]` once the server
    /// group is named `nosuch`, which the inventory lacks.
    ///
    /// What would make this red: no `extract` filter (the loop fails `unknown filter`, which is
    /// how the whole playbook stopped), or an `extract` that does not read `hostvars` by host
    /// name.
    #[test]
    fn the_k3s_node_cidr_loop_extracts_each_host_s_address() {
        let t = Templar::new(std::env::temp_dir());
        let loop_text = r"{{
          (
            groups[server_group] | default([])
            + groups[agent_group] | default([])
          )
          | map('extract', hostvars)
          | selectattr('ansible_facts.default_ipv4', 'defined')
          | map(attribute='ansible_facts.default_ipv4.address')
          | flatten | unique | list
        }}";
        let view = Arc::new(vars(json!({
            "h1": {"ansible_facts": {"default_ipv4": {"address": "192.0.2.1"}}},
            "h2": {"ansible_facts": {}},
            "h3": {"ansible_facts": {"default_ipv4": {"address": "192.0.2.3"}}},
        })));
        let shared = Arc::new(vars(
            json!({"groups": {"server": ["h1"], "agent": ["h2", "h3"]}}),
        ));
        for (server_group, want) in [
            ("server", json!(["192.0.2.1", "192.0.2.3"])),
            ("nosuch", json!(["192.0.2.3"])),
        ] {
            let map = vars(json!({"server_group": server_group, "agent_group": "agent"}));
            let v = Vars {
                map: &map,
                hostvars: Some(&view),
                shared: Some(&shared),
                untrusted: None,
                untrusted_hosts: None,
            };
            assert_eq!(t.render(loop_text, v).unwrap(), want, "{server_group}");
        }
    }

    /// `extract` reads `hostvars` through the same object a bare `hostvars[h]` goes through, so
    /// a host holding a value that came from a managed host makes the render data either way:
    /// its `{{ 1 + 1 }}` comes back as text, never evaluated. The same value from an author, on
    /// a host with nothing untrusted, is rendered again as it would be read bare.
    ///
    /// What would make this red: `extract` reading the hosts' map behind the view's back, which
    /// would skip the taint and evaluate a host's string here.
    #[test]
    fn extract_on_hostvars_taints_like_a_bare_read() {
        let t = Templar::new(std::env::temp_dir());
        let view = Arc::new(vars(json!({"h2": {"x": "{{ 1 + 1 }}"}})));
        let empty = Map::new();
        let hosts = BTreeSet::from(["h2".to_string()]);
        let mut wrong = Vec::new();
        for (untrusted_hosts, want) in [(Some(&hosts), json!("{{ 1 + 1 }}")), (None, json!(2))] {
            let v = Vars {
                map: &empty,
                hostvars: Some(&view),
                shared: None,
                untrusted: None,
                untrusted_hosts,
            };
            for text in [
                "{{ hostvars['h2'].x }}",
                "{{ 'h2' | extract(hostvars, 'x') }}",
                "{{ ['h2'] | map('extract', hostvars, ['x']) | first }}",
            ] {
                let got = t.render(text, v);
                if got.as_ref() != Ok(&want) {
                    wrong.push(format!("{text} -> {got:?}"));
                }
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// A map that carries a `hostvars` key of its own, with no view beside it, still reads from
    /// the map: the fixtures above and every caller that passes a bare `&Map` depend on it.
    #[test]
    fn a_map_without_a_view_still_answers_from_the_map() {
        let t = Templar::new(std::env::temp_dir());
        assert_eq!(
            t.render("{{ hostvars['a'].y }}", &hostvars_map()).unwrap(),
            json!(1)
        );
    }

    #[test]
    fn conditions_need_a_boolean_and_accept_stray_braces() {
        let t = Templar::new(std::env::temp_dir());
        assert!(t.condition("x > 3", &vars(json!({"x": 5}))).unwrap());
        assert!(!t.condition("x > 3", &vars(json!({"x": 1}))).unwrap());
        assert!(t.condition("{{ x > 3 }}", &vars(json!({"x": 5}))).unwrap());
        let err = t.condition("'yes'", &Map::new()).unwrap_err();
        assert!(
            err.0.contains("Conditionals must have a boolean result"),
            "{err}"
        );
        assert!(t.condition("missing > 3", &Map::new()).is_err());
    }

    /// A delimited condition whose render is an **author** string is evaluated again, measured
    /// on ansible-core 2.19.12 for `when` and `assert` alike: `healthy: "true"` makes
    /// `when: "{{ healthy }}"` run the task, `cond_s: "1 == 1"` makes it pass, and
    /// `hello: "hello"` fails the boolean rule. A string a host wrote is never evaluated again:
    /// with a registered `s.stdout` of `1 == 2` the reference refuses `"{{ s.stdout }}"` with
    /// `Encountered untrusted template or expression.`, and bare `s.stdout` fails the boolean
    /// rule. The payload here is `1 == 1`, so compiling it would answer `true`.
    ///
    /// What would make this red: the second evaluation not happening (`healthy` refused), or
    /// happening without reading the first pass's taint, so that a registered result, another
    /// host's untrusted variable or a lookup's text is compiled on the controller.
    #[test]
    fn a_delimited_condition_reads_an_author_string_again_and_never_a_host_one() {
        let t = Templar::new(std::env::temp_dir());
        let map = vars(json!({
            "healthy": "true",
            "cond_s": "1 == 1",
            "hello": "hello",
            "s": {"stdout": "1 == 1"},
        }));
        let view = Arc::new(vars(json!({"h2": {"x": "1 == 1"}})));
        assert!(t.condition("{{ healthy }}", &map).unwrap());
        assert!(t.condition("{{ cond_s }}", &map).unwrap());
        let boolean = "Conditionals must have a boolean result";
        let err = t.condition("{{ hello }}", &map).unwrap_err();
        assert!(err.0.contains(boolean), "{err}");
        // Bare, the string is the result and nothing is evaluated again.
        let err = t.condition("healthy", &map).unwrap_err();
        assert!(err.0.contains(boolean), "{err}");

        let registered = BTreeSet::from(["s".to_string()]);
        let hosts = BTreeSet::from(["h2".to_string()]);
        let from_host = Vars {
            map: &map,
            hostvars: Some(&view),
            shared: None,
            untrusted: Some(&registered),
            untrusted_hosts: Some(&hosts),
        };
        for text in [
            "{{ s.stdout }}",
            "{{ hostvars['h2'].x }}",
            "{{ lookup('env', 'PATH') }}",
        ] {
            let err = t.condition(text, from_host).unwrap_err();
            assert_eq!(
                err.0, "Encountered untrusted template or expression.",
                "{text}"
            );
        }
        let err = t.condition("s.stdout", from_host).unwrap_err();
        assert!(err.0.contains(boolean), "{err}");
        // The author's strings stay author strings beside a host's.
        assert!(t.condition("{{ cond_s }}", from_host).unwrap());
    }

    /// Every filter the engine registers, with the arguments it needs, minijinja's builtins
    /// included, separated by `; `.
    const FILTERS: &str = "bool; int; float; ternary(1, 2); combine({}); dict2items; \
        items2dict; to_json; to_nice_json; from_json; basename; dirname; split(','); \
        regex_replace('a', 'b'); regex_search('a'); regex_findall('a'); b64decode; b64encode; \
        comment; difference([1]); intersect([1]); union([1]); flatten; from_yaml; to_uuid; \
        quote; regex_escape; extract({}); ansible.utils.ipwrap; to_yaml; to_nice_yaml; safe; \
        escape; e; lower; upper; title; capitalize; replace('a', 'b'); length; count; dictsort; \
        items; reverse; trim; join(','); lines; round; abs; attr('a'); first; last; min; max; \
        sort; list; string; batch(2); slice(2); sum; indent; select; reject; selectattr('a'); \
        rejectattr('a'); map('upper'); groupby('a'); unique; chain([1]); zip([1]); pprint; \
        format; ansible.builtin.dict2items; ansible.builtin.length";

    /// The same for tests.
    const TESTS: &str = "truthy; falsy; match('a'); search('a'); regex('a'); contains(1); \
        changed; failed; succeeded; skipped; version('1', '>'); none; boolean; odd; even; \
        divisibleby(2); number; integer; int; float; string; sequence; iterable; mapping; \
        startingwith('a'); endingwith('a'); lower; upper; sameas(1); eq(1); equalto(1); ne(1); \
        lt(1); le(1); gt(1); ge(1); in([1]); true; false; filter; test; safe; escaped; \
        ansible.builtin.none";

    /// Measured on ansible-core 2.19.12: a filter given an undefined value does not run, its
    /// result is undefined as well, so `nope | dict2items` fails as an undefined read (the one a
    /// skipped task's `loop:` may raise) and `nope | dict2items | default('x')` is `x`. A test
    /// given one fails as an undefined read too (`nope is none`: `'nope' is undefined`). Red if
    /// a filter answers with its own type error (`dict2items requires a dictionary`) or a test
    /// judges the undefined value.
    #[test]
    fn an_undefined_input_stays_undefined_through_a_filter_or_a_test() {
        let t = Templar::new(std::env::temp_dir());
        let r = |text: String| t.render(&text, &Map::new());
        for filter in FILTERS.split("; ") {
            let err = r(format!("{{{{ nope | {filter} }}}}")).unwrap_err();
            assert!(err.is_undefined(), "{filter}: {err}");
            assert_eq!(
                r(format!("{{{{ nope | {filter} | default('x') }}}}")),
                Ok(json!("x")),
                "{filter}"
            );
            assert_eq!(
                r(format!("{{{{ nope | {filter} is defined }}}}")),
                Ok(json!(false)),
                "{filter}"
            );
        }
        for test in TESTS.split("; ") {
            let err = r(format!("{{{{ nope is {test} }}}}")).unwrap_err();
            assert!(err.is_undefined(), "{test}: {err}");
        }
    }

    /// The filters and tests that exist to look at an undefined value keep seeing it, measured
    /// on ansible-core 2.19.12: `default` and `d` answer their fallback, `type_debug` says
    /// `UndefinedMarker`, `defined` and `undefined` answer, and `mandatory` fails with its own
    /// message rather than as an undefined read. Red if the short-circuit above catches them.
    #[test]
    fn the_filters_and_tests_made_for_undefined_still_see_it() {
        let t = Templar::new(std::env::temp_dir());
        for (text, want) in [
            ("{{ nope | default('x') }}", json!("x")),
            ("{{ nope | d('x') }}", json!("x")),
            ("{{ nope | ansible.builtin.default('x') }}", json!("x")),
            ("{{ nope | type_debug }}", json!("UndefinedMarker")),
            ("{{ nope is defined }}", json!(false)),
            ("{{ nope is ansible.builtin.defined }}", json!(false)),
            ("{{ nope is undefined }}", json!(true)),
            (
                "{{ [{'a': 1}, {}] | selectattr('a', 'defined') | list }}",
                json!([{"a": 1}]),
            ),
        ] {
            assert_eq!(t.render(text, &Map::new()), Ok(want), "{text}");
        }
        let err = t.render("{{ nope | mandatory }}", &Map::new()).unwrap_err();
        assert!(!err.is_undefined(), "{err}");
        assert!(err.0.contains("Mandatory variable"), "{err}");
    }

    /// An undefined value inside a literal, handed to a filter or a lookup as an argument, or
    /// dumped, never comes out as `null` or `""`. Each of these is an undefined read on
    /// ansible-core 2.19.12 (the `map(attribute='x')` one fails there with `object of type
    /// 'dict' has no attribute 'x'`).
    /// Red if the task succeeds with the hole filled in, as `set_fact: {pk: "{{ [nope | lower,
    /// 'curl'] }}"}` storing `[null, "curl"]` would.
    #[test]
    fn a_value_holding_an_undefined_one_is_an_undefined_read() {
        let t = Templar::new(std::env::temp_dir());
        let vars = vars(json!({"items_": [{"x": 1}, {"y": 2}], "items": [1]}));
        for text in [
            "{{ [nope | lower] }}",
            "{{ {'k': nope | int} }}",
            "{{ dict(k=nope | lower) }}",
            "{{ [1] + [nope | lower] }}",
            "{{ {'a': 1} | combine({'k': nope | lower}) }}",
            "{{ [1] | union(nope | list) }}",
            "{{ '%s' | format(nope | lower) }}",
            "{{ [nope | lower] | to_json }}",
            "{{ [nope | lower] | to_nice_json }}",
            "{{ [nope | lower] | to_nice_yaml }}",
            "{{ lookup('env', nope | lower) }}",
            "{{ lookup('vars', nope | lower, default='z') }}",
            "{{ lookup('vars', 'zz', default=nope | lower) }}",
            "{{ 'a' | regex_replace('a', nope | lower) }}",
            "{{ 'a' | regex_replace('a', 'b', ignorecase=nope | bool) }}",
            "{{ [1, 2] | join(nope | lower) }}",
            "{{ 'a' is match(nope | lower) }}",
            "{{ 1 is eq(nope) }}",
            "{{ 1 is ne(nope | lower) }}",
            "{{ [nope] }}",
            "{{ items_ | map(attribute='x') | list }}",
            "{{ items + [nope | lower] }}",
            "{{ {'k': nope} | dict2items }}",
            "{{ [{'key': 'a', 'value': nope}] | items2dict }}",
            "{{ [[nope]] | flatten }}",
            "{{ {'a': 1} | combine({'k': nope}) }}",
            "{{ {'k': nope} | default('x', true) }}",
            "{{ [nope] | first }}",
            "{{ true | ternary(nope, 'b') }}",
            "x {{ [nope | lower] }}",
            "x {{ {'k': nope} }}",
        ] {
            let err = t.render(text, &vars).expect_err(&format!("leaked: {text}"));
            assert!(err.is_undefined(), "{text}: {err}");
        }
    }

    /// Measured on ansible-core 2.19.12: each of these fails as an undefined read, and each
    /// rendered a value here before - the last one, the shape of a k3s role building its server
    /// URLs, wrote a partial list and reported the task green.
    #[test]
    fn a_text_filter_over_a_container_holding_an_undefined_fails_the_read() {
        let t = Templar::new(std::env::temp_dir());
        let vars = vars(json!({"hs": ["a", "b"], "hv": {"a": {"ip": "10.0.0.1"}, "b": {}}}));
        for text in [
            "{{ [1, nope] | join(',') }}",
            "{{ [1, nope] | string }}",
            "{{ [1, nope] | map('quote') | join(' ') }}",
            "{{ [1, nope] | sum }}",
            "{{ hs | map('extract', hv, 'ip') | map('regex_replace', '^(.*)$', 'https://\\1:6443') | join(',') }}",
        ] {
            let err = t.render(text, &vars).expect_err(&format!("leaked: {text}"));
            assert!(err.is_undefined(), "{text}: {err}");
        }
        // The two the reference answers, unchanged.
        assert_eq!(t.render("{{ [nope] | length }}", &vars), Ok(json!(1)));
        assert_eq!(t.render("{{ [1, nope] | first }}", &vars), Ok(json!(1)));
    }

    /// Measured on ansible-core 2.19.12: `'x' ~ [1, nope]` is an undefined read. minijinja 2.24
    /// concatenates by `Display` inside its VM and only checks that neither side is undefined
    /// itself, so the list prints as `[1, undefined]`; no environment hook reaches it.
    #[test]
    #[ignore = "minijinja 2.24 offers no hook on the ~ operator for a list holding undefined"]
    fn a_concatenation_with_a_container_holding_an_undefined_fails_the_read() {
        let t = Templar::new(std::env::temp_dir());
        let err = t
            .render("{{ 'x' ~ [1, nope] }}", &Map::new())
            .expect_err("leaked: {{ 'x' ~ [1, nope] }}");
        assert!(err.is_undefined(), "{err}");
    }

    /// The idioms roles write around undefined values, each measured on ansible-core 2.19.12.
    /// Red if the short-circuit reaches one of them: an argument checked where the reference
    /// does not (`ternary`, `default`), a list's contents checked where only the result is
    /// (`length`), or a test that exists to see undefined values catching them.
    #[test]
    fn the_idioms_around_undefined_values_answer_as_the_reference() {
        let t = Templar::new(std::env::temp_dir());
        let omit = crate::vars::omit_token();
        let vars = vars(json!({
            "items_": [{"x": 1}, {"y": 2}], "item": {"y": 2}, "s": "a,b", "y": "3", "omit": omit,
        }));
        for (text, want) in [
            ("{{ nope | default(omit) }}", json!(omit)),
            ("{{ item.x | default(omit) }}", json!(omit)),
            ("{{ (nope | default({})).get('k') }}", json!(null)),
            ("{{ (nope | default({})).k | default('z') }}", json!("z")),
            ("{{ nope is defined and nope | bool }}", json!(false)),
            (
                "{{ lookup('env', 'VOLANT_NOPE_X') | default('d', true) }}",
                json!("d"),
            ),
            ("{{ nope | d(y) | int }}", json!(3)),
            ("{{ false and nope | bool }}", json!(false)),
            ("{{ true or nope | bool }}", json!(true)),
            ("{{ true | ternary('a', nope) }}", json!("a")),
            ("{{ false | ternary(nope, 'b') }}", json!("b")),
            ("{{ true | ternary('a', nope | lower) }}", json!("a")),
            ("{{ 'x' | default(nope) }}", json!("x")),
            ("{{ 'x' | default(nope | lower) }}", json!("x")),
            ("{{ [nope] | type_debug }}", json!("list")),
            ("{{ [nope] | length }}", json!(1)),
            ("{{ [1, nope] | first }}", json!(1)),
            ("x {{ 'a' if false }}", json!("x ")),
            ("{{ [nope | lower] | length }}", json!(1)),
            ("{{ {'k': nope} | length }}", json!(1)),
            ("{{ nope | default([]) | length }}", json!(0)),
            (
                "{{ items_ | selectattr('x', 'defined') | list }}",
                json!([{"x": 1}]),
            ),
            (
                "{{ items_ | map(attribute='x') | select('defined') | list }}",
                json!([1]),
            ),
            ("{{ nope is defined and nope | length > 0 }}", json!(false)),
            (
                "{{ nope is not defined or nope | length == 0 }}",
                json!(true),
            ),
            ("{{ nope | default('') | length }}", json!(0)),
            ("{{ nope | d({}) | dict2items }}", json!([])),
            (
                "{{ items_ | map(attribute='x', default=0) | list }}",
                json!([1, 0]),
            ),
            ("{{ (nope is undefined) | ternary('u', 'd') }}", json!("u")),
            (
                "{{ items_ | selectattr('x', 'undefined') | list }}",
                json!([{"y": 2}]),
            ),
            (
                "{{ items_ | rejectattr('x', 'undefined') | list }}",
                json!([{"x": 1}]),
            ),
            ("{{ nope | default(false) | bool }}", json!(false)),
            ("{{ (nope | default([])) + [1] }}", json!([1])),
            ("{{ {'a': nope | default('z')} }}", json!({"a": "z"})),
            ("{{ lookup('vars', 'nope', default='z') }}", json!("z")),
            ("{{ s.split(',') | map('trim') | list }}", json!(["a", "b"])),
            (
                "{{ items_ | map(attribute='x') | select('defined') | map('string') | join(',') }}",
                json!("1"),
            ),
        ] {
            assert_eq!(t.render(text, &vars), Ok(want), "{text}");
        }
    }

    /// Every filter and test minijinja ships goes through `add_filter`/`add_test`, which is what
    /// the `ansible.builtin.` alias proves: a builtin a minijinja update adds without it being
    /// listed in `filters::register` would judge an undefined input again. The names are read
    /// off a bare environment's `Debug` output, the only place minijinja lists them.
    #[test]
    fn every_minijinja_builtin_goes_through_the_wrapper() {
        let bare = format!("{:?}", Environment::new());
        let ours = format!("{:?}", Templar::new(std::env::temp_dir()).env);
        let names = |debug: &str, field: &str| -> Vec<String> {
            let start = debug.find(&format!("{field}: [")).expect(field) + field.len() + 3;
            let end = start + debug[start..].find(']').expect(field);
            debug[start..end]
                .split(", ")
                .map(|n| n.trim_matches('"').to_string())
                .collect()
        };
        for field in ["tests", "filters"] {
            let registered = names(&ours, field);
            let builtins = names(&bare, field);
            assert!(builtins.len() > 10, "{field}: {builtins:?}");
            for name in builtins {
                assert!(
                    registered.contains(&format!("ansible.builtin.{name}")),
                    "{field} {name} is not wrapped"
                );
            }
        }
    }
}
