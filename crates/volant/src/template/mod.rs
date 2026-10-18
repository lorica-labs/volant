// SPDX-License-Identifier: GPL-3.0-or-later
//! Jinja2 templating the way ansible-core 2.19 does it: strict about undefined variables, and a
//! template that is one expression yields that expression's value, not its text.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use minijinja::functions::Function;
use minijinja::machinery::{
    CodeGenerator, CompiledTemplate, Instruction, Instructions, TemplateConfig, Vm,
    WhitespaceConfig, make_string_output, parse_expr,
};
use minijinja::syntax::SyntaxConfig;
use minijinja::value::{Enumerator, FunctionArgs, FunctionResult, Object, Rest, ValueKind};
use minijinja::{AutoEscape, Environment, ErrorKind, State, UndefinedBehavior};
use serde_json::{Map, Value};

use crate::bridge::{Bridge, PluginKind};

mod bridged;
mod file;
mod filters;
pub(crate) use filters::python_json;
mod lookups;
mod tests;
mod yaml_dump;

pub use bridged::{bridged_name, not_sent_reason};
pub use file::FileRender;
pub use lookups::supported as lookup_supported;

/// Prefix of every error raised because a variable had no value, the one template failure
/// Ansible treats as recoverable in places such as `vars_files`.
const UNDEFINED: &str = "The task includes an option with an undefined variable.";

/// What [`Templar::render_map_tainted`] puts before a plugin's failure, then the key in quotes.
pub(crate) const RESOLVING: &str = "Error while resolving value for ";

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

/// The global every `~` is compiled to call, see [`route_calls`]. Unwritable in Jinja for the
/// same reason as `TAINT_KEY`, and `Context` answers nothing for it, so no variable shadows it.
/// What `route_calls` puts in place of the name in `NAME is vault_encrypted`, before the name.
pub(crate) const VAULT_NAME: &str = "volant::vault_name:";

const CONCAT_KEY: &str = "volant::concat";
const ITEM_KEY: &str = "volant::item";
/// Prefix of the name an attribute lookup is routed through; the context answers it with a
/// function that reads that attribute.
const ATTR_KEY: &str = "volant::attr:";

/// The global a call to `query` or `q` is compiled to call, see [`route_calls`]. Not a global of
/// either name: measured on ansible-core 2.19.12, `q is defined` is false on a host with no
/// variable `q`.
pub(crate) const QUERY_KEY: &str = "volant::query";

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
    /// The host's gathered facts, a layer shared rather than copied into `map` for every task.
    /// Consulted after `shared` and before `map`: whoever builds it leaves out every fact a layer
    /// of `map` outranks, so the facts left answer ahead of the layers below them.
    pub facts: Option<&'a Arc<Map<String, Value>>>,
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
            facts: None,
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
    facts: Option<Arc<Map<String, Value>>>,
    untrusted: BTreeSet<String>,
    untrusted_hosts: BTreeSet<String>,
    tainted: Tainted,
    memo: Memo,
}

impl Object for Context {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let key = key.as_str()?;
        // Not a variable: the VM looks the root up before the globals, so a variable of that
        // name (`-e '{"volant::concat": 1}'`) would otherwise replace every `~`.
        if key == CONCAT_KEY || key == QUERY_KEY || key == ITEM_KEY {
            return None;
        }
        if let Some(attr) = key.strip_prefix(ATTR_KEY) {
            return Some(chained_attr(attr.to_string()));
        }
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
            // Between the two: the facts that answer, the ones no layer of the map outranks.
            if let Some(facts) = &self.facts
                && let Some(value) = facts.get(key)
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
        for key in self.facts.iter().flat_map(|f| f.keys()) {
            if !self.vars.contains_key(key)
                && !self.shared.as_ref().is_some_and(|s| s.contains_key(key))
            {
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
        facts: vars.facts.cloned(),
        untrusted: vars.untrusted.cloned().unwrap_or_default(),
        untrusted_hosts: vars.untrusted_hosts.cloned().unwrap_or_default(),
        tainted: Arc::clone(&tainted),
        memo: Memo::default(),
    });
    (ctx, tainted)
}

pub struct Templar {
    env: RwLock<Bridged>,
    bridge: Option<Arc<Bridge>>,
}

/// The environment a render runs in, and the filter and test names already looked at.
///
/// A name the engine has no native implementation of is registered here, as a call to the
/// Python helper, the first time a template uses it. The environment is replaced rather than
/// changed under a render that holds it: a render keeps its own `Arc` and takes no lock, so a
/// lookup rendering a template of its own never waits for itself.
struct Bridged {
    env: Arc<Environment<'static>>,
    seen: HashSet<(PluginKind, String)>,
}

impl Templar {
    /// `base_dir` anchors relative paths in lookups such as `lookup('file', ...)`. No Python
    /// helper: a filter, test or lookup the engine lacks is unknown.
    pub fn new(base_dir: PathBuf) -> Self {
        Self::with_bridge(base_dir, None)
    }

    /// The bridge to ansible-core this templar sends what it lacks to, when it has one.
    pub(crate) fn bridge(&self) -> Option<&Arc<Bridge>> {
        self.bridge.as_ref()
    }

    /// [`Templar::new`], sending what the engine lacks to `bridge`: a filter or test the engine
    /// has no native implementation of, a lookup it does not run. A native name never reaches it.
    pub fn with_bridge(base_dir: PathBuf, bridge: Option<Arc<Bridge>>) -> Self {
        Self {
            env: RwLock::new(Bridged {
                env: Arc::new(environment(base_dir, bridge.as_ref())),
                seen: HashSet::new(),
            }),
            bridge,
        }
    }

    /// The environment as it stands.
    pub(crate) fn env(&self) -> Arc<Environment<'static>> {
        Arc::clone(&self.env.read().unwrap_or_else(PoisonError::into_inner).env)
    }

    /// The environment to run `code` in: [`Templar::env`], plus a call to the bridge under each
    /// filter and test name `code` uses that the engine has no native implementation of.
    fn env_for<'a, 's: 'a>(
        &self,
        code: impl IntoIterator<Item = &'a Instructions<'s>>,
    ) -> Arc<Environment<'static>> {
        let Some(bridge) = &self.bridge else {
            return self.env();
        };
        let mut used = Vec::new();
        for instructions in code {
            // `map('f')`, `select('t')`, `selectattr('a', 't')`...: the filter or test they name
            // is a string constant, so each constant that reads as a name is a candidate.
            let mut by_name = (false, false);
            let mut constants = Vec::new();
            let mut idx = 0;
            while let Some(op) = instructions.get(idx) {
                match op {
                    Instruction::ApplyFilter(name, ..) => {
                        let name = bridged::normalized(name);
                        match bridged::short(&name) {
                            "map" => by_name.0 = true,
                            "select" | "reject" | "selectattr" | "rejectattr" => by_name.1 = true,
                            _ => {}
                        }
                        used.push((PluginKind::Filter, name));
                    }
                    Instruction::PerformTest(name, ..) => {
                        used.push((PluginKind::Test, bridged::normalized(name)));
                    }
                    Instruction::LoadConst(value) => {
                        if let Some(text) = value.as_str().filter(|t| bridged::is_name(t)) {
                            constants.push(text.to_string());
                        }
                    }
                    _ => {}
                }
                idx += 1;
            }
            for text in constants {
                if by_name.0 {
                    used.push((PluginKind::Filter, text.clone()));
                }
                if by_name.1 {
                    used.push((PluginKind::Test, text));
                }
            }
        }
        {
            let current = self.env.read().unwrap_or_else(PoisonError::into_inner);
            if used.iter().all(|name| current.seen.contains(name)) {
                return Arc::clone(&current.env);
            }
        }
        let mut current = self.env.write().unwrap_or_else(PoisonError::into_inner);
        let Bridged { env, seen } = &mut *current;
        for (kind, name) in used {
            if seen.contains(&(kind, name.clone())) {
                continue;
            }
            if bridged::sendable_name(kind, &name) && !bridged::knows(env, kind, &name) {
                bridged::register(Arc::make_mut(env), bridge, kind, &name);
            }
            seen.insert((kind, name));
        }
        Arc::clone(env)
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
        // Whitespace around the expression, a final newline included, is text: the reference
        // keeps it, so the result is a string.
        if text.trim() == text
            && let Some(expr) = single_expression(text)
        {
            return self.evaluate_in(expr, ctx);
        }
        let ws = WhitespaceConfig {
            keep_trailing_newline: true,
            ..WhitespaceConfig::default()
        };
        render_str(self, "<string>", text, ctx, ws)
            .map(Value::String)
            .map_err(convert_error)
    }

    /// `render`, one pass: a result that is itself a template is not rendered again, which is how
    /// the reference templates a play's `vars_prompt` before it asks.
    pub(crate) fn render_one_pass<'a>(
        &self,
        text: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<Value, TemplateError> {
        let (ctx, _tainted) = context_of(vars.into());
        self.render_once(text, &ctx)
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
            let rendered = self
                .render_value_in(value, &ctx, &tainted, &mut any)
                .map_err(|err| {
                    // Measured on ansible-core 2.19.12: a plugin's failure names the value it
                    // was resolving, `Error while resolving value for 'msg': The lookup plugin
                    // 'x' failed: …`.
                    if bridged::is_plugin_failure(&err.0) {
                        TemplateError(format!("{RESOLVING}'{key}': {}", err.0))
                    } else if err.0.starts_with(filters::UNDECRYPTABLE) {
                        TemplateError(format!(
                            "{RESOLVING}'{key}': Error rendering template: {}",
                            err.0
                        ))
                    } else {
                        err
                    }
                })?;
            out.insert(key.clone(), rendered);
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

    /// `evaluate`, and whether the value is data: a lookup read a file, a command or the
    /// environment, or the expression read a name that came from a managed host.
    pub fn evaluate_tainted<'a>(
        &self,
        expr: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<(Value, bool), TemplateError> {
        let (ctx, tainted) = context_of(vars.into());
        let value = self.evaluate_in(expr, &ctx)?;
        Ok((value, tainted.load(Ordering::Relaxed)))
    }

    fn evaluate_in(&self, expr: &str, ctx: &minijinja::Value) -> Result<Value, TemplateError> {
        let value = eval_expr(self, expr, ctx).map_err(convert_error)?;
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
        // What `!unsafe` loaded as is text from here on: a sibling that reads it must see the
        // text, not the wrapper, and the name is data so no pass renders it.
        for (k, v) in &mut current {
            if matches!(v, Value::String(s) if crate::yaml::vault_text(s).is_some()) {
                crate::vault::note_name(k);
            }
            if !untrusted.contains(k) && unwrap_unsafe(v) {
                untrusted.insert(k.clone());
            }
        }
        for _ in 0..5 {
            // One context for the whole pass: every value of the map renders against the same
            // map, so building it per value paid for the same conversion once per variable.
            let (ctx, tainted) = context_of(Vars {
                map: &current,
                hostvars: vars.hostvars,
                shared: vars.shared,
                facts: vars.facts,
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

/// The engine's environment: minijinja's, strict about undefined values, with every filter,
/// test and lookup of the engine; the lookups the engine lacks go to `bridge`.
fn environment(base_dir: PathBuf, bridge: Option<&Arc<Bridge>>) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    // Strict mode refuses to print an undefined value, not a list or mapping holding one,
    // which minijinja prints as `[undefined]`. Measured on ansible-core 2.19.12,
    // `x {{ [nope] }}` is an undefined read.
    //
    // Inside text, measured on the same version: a list or mapping prints as Python's `repr`
    // does, and a `none` that is a whole part of the text prints nothing.
    env.set_formatter(|out, state, value| {
        if !value.is_undefined() && holds_undefined(value) {
            return Err(minijinja::Error::from(ErrorKind::UndefinedError));
        }
        if value.is_none() {
            return Ok(());
        }
        match container_repr(value) {
            Some(text) => out.write_str(&text).map_err(|_| {
                minijinja::Error::new(ErrorKind::WriteFailure, "failed to write output")
            }),
            None => minijinja::escape_formatter(out, state, value),
        }
    });
    filters::register(&mut env, base_dir, bridge);
    env.add_function(CONCAT_KEY, concat);
    env.add_function(ITEM_KEY, chained_item);
    env
}

/// Python's `repr` of a list or mapping, `None` for any other value.
fn container_repr(value: &minijinja::Value) -> Option<String> {
    if !matches!(
        value.kind(),
        ValueKind::Seq | ValueKind::Map | ValueKind::Iterable
    ) {
        return None;
    }
    let json = serde_json::to_value(value).ok()?;
    Some(crate::playbook::python_repr(&json))
}

/// Attribute access that leaves an undefined parent undefined, as the reference does until
/// `default`, `is defined` or a use decides. The VM's own lookup fails on an undefined parent.
fn chained_attr(name: String) -> minijinja::Value {
    minijinja::Value::from_function(
        move |parent: minijinja::Value| -> Result<minijinja::Value, minijinja::Error> {
            if parent.is_undefined() {
                return Ok(minijinja::Value::UNDEFINED);
            }
            parent.get_attr(&name)
        },
    )
}

/// `parent[key]`, chained like [`chained_attr`].
fn chained_item(
    parent: minijinja::Value,
    key: minijinja::Value,
) -> Result<minijinja::Value, minijinja::Error> {
    if parent.is_undefined() {
        return Ok(minijinja::Value::UNDEFINED);
    }
    parent.get_item(&key)
}

/// The function name an attribute lookup is routed through: `ATTR_KEY` then the attribute.
/// Leaked once per distinct name, because compiled code holds names for as long as it lives.
fn attr_call(name: &str) -> &'static str {
    static NAMES: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    let full = format!("{ATTR_KEY}{name}");
    let mut names = NAMES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(known) = names.get(full.as_str()) {
        return known;
    }
    let leaked: &'static str = Box::leak(full.into_boxed_str());
    names.insert(leaked);
    leaked
}

/// Replaces every `!unsafe` wrapper inside `value` by its text, and every `!vault` wrapper that
/// decrypts by what it holds; whether there was one. A vault wrapper that does not decrypt stays
/// as it is, and raises when something renders it.
fn unwrap_unsafe(value: &mut Value) -> bool {
    match value {
        Value::String(s) => {
            let text = crate::yaml::unsafe_text(s)
                .or_else(|| crate::yaml::vault_text(s).and_then(|c| crate::vault::reveal(&c).ok()));
            text.map(|t| *s = t).is_some()
        }
        Value::Array(items) => items.iter_mut().fold(false, |a, v| unwrap_unsafe(v) | a),
        Value::Object(map) => map.values_mut().fold(false, |a, v| unwrap_unsafe(v) | a),
        _ => false,
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

/// `{{ expr }}` and nothing else: no text around, no second expression, no statement. A `}}`
/// inside is no end when it closes a bracket the expression opened or sits in a string literal:
/// `{'a': {'x': 5}}` closing on `}}}` and `'}}'` are one expression, `1 }}{{ 2` is two.
fn single_expression(text: &str) -> Option<&str> {
    let t = text.trim();
    let inner = t.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{%") || inner.contains("{#") || !closes_only_what_it_opens(inner) {
        return None;
    }
    Some(inner.trim())
}

/// Whether every bracket `expr` closes outside a string literal is one it opened. minijinja's
/// own parser cannot be asked: its lexer panics on a `}` with nothing open.
fn closes_only_what_it_opens(expr: &str) -> bool {
    let mut depth = 0usize;
    let mut quote = None;
    let mut chars = expr.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(_), '\\') => {
                chars.next();
            }
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '{' | '[' | '(') => depth += 1,
            (None, '}' | ']' | ')') => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            _ => {}
        }
    }
    true
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
    env.add_filter(format!("ansible.legacy.{name}"), filter.clone());
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
    env.add_filter(format!("ansible.legacy.{name}"), filter.clone());
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
    env.add_test(format!("ansible.legacy.{name}"), test.clone());
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

/// `a ~ b`. minijinja's own `~` only refuses an operand that is undefined itself and prints a
/// list or mapping holding one as `[1, undefined]`; measured on ansible-core 2.19.12,
/// `'x' ~ [1, nope]` and `'x' ~ {'k': nope}` are undefined reads, like `'x' ~ nope`.
fn concat(
    left: minijinja::Value,
    right: minijinja::Value,
) -> Result<minijinja::Value, minijinja::Error> {
    if holds_undefined(&left) || holds_undefined(&right) {
        return Err(minijinja::Error::from(ErrorKind::UndefinedError));
    }
    let shown = |v: &minijinja::Value| container_repr(v).unwrap_or_else(|| v.to_string());
    Ok(minijinja::Value::from(format!(
        "{}{}",
        shown(&left),
        shown(&right)
    )))
}

/// Replaces each `~` in compiled code by a call to [`concat()`], and each call to `query` or `q`
/// by a call to [`QUERY_KEY`]. minijinja evaluates `~` in its VM with no hook, and its syntax tree
/// cannot be rewritten in place, so the swap happens on the bytecode: the concatenation
/// instruction and a two-argument call take the same two operands off the stack in the same
/// order, and a call keeps its own, so every jump target stays where it was.
fn route_calls(instructions: &mut Instructions<'_>) {
    let mut idx = 0;
    loop {
        let named = match (instructions.get(idx), instructions.get(idx + 1)) {
            (Some(Instruction::Lookup(name)), Some(Instruction::PerformTest(test, ..)))
                if bridged::short(test) == "vault_encrypted" =>
            {
                Some(format!("{VAULT_NAME}{name}"))
            }
            _ => None,
        };
        let Some(op) = instructions.get_mut(idx) else {
            break;
        };
        if let Some(named) = named {
            *op = Instruction::LoadConst(minijinja::Value::from(named));
        }
        match op {
            Instruction::StringConcat => *op = Instruction::CallFunction(CONCAT_KEY, Some(2)),
            Instruction::GetItem => *op = Instruction::CallFunction(ITEM_KEY, Some(2)),
            Instruction::GetAttr(name) => {
                *op = Instruction::CallFunction(attr_call(name), Some(1));
            }
            Instruction::CallFunction(name, args) if matches!(*name, "query" | "q") => {
                *op = Instruction::CallFunction(QUERY_KEY, *args);
            }
            _ => {}
        }
        idx += 1;
    }
}

/// `env.render_named_str`, with `~` routed through [`concat()`]. Compiled once per call, as
/// `render_named_str` does, with the whitespace settings given. The only place that decides
/// auto-escape and syntax. The environment is the templar's, with the filters and tests the
/// compiled code calls that only the Python helper has ([`Templar::env_for`]).
fn render_str(
    templar: &Templar,
    name: &str,
    source: &str,
    ctx: &minijinja::Value,
    ws_config: WhitespaceConfig,
) -> Result<String, minijinja::Error> {
    let config = TemplateConfig {
        syntax_config: SyntaxConfig,
        ws_config,
        // Never by file name: measured on ansible-core 2.19.12, `index.html.j2` holding
        // `{{ "<b>" }}` writes `<b>`.
        default_auto_escape: Arc::new(|_| AutoEscape::None),
    };
    let mut compiled = CompiledTemplate::new(name, source, &config)?;
    route_calls(&mut compiled.instructions);
    compiled.blocks.values_mut().for_each(route_calls);
    let env =
        templar.env_for(std::iter::once(&compiled.instructions).chain(compiled.blocks.values()));
    let mut out = String::with_capacity(compiled.buffer_size_hint);
    Vm::new(&env).eval(
        &compiled.instructions,
        ctx.clone(),
        &compiled.blocks,
        &mut make_string_output(&mut out),
        compiled.initial_auto_escape,
    )?;
    Ok(out)
}

/// `env.compile_expression(expr)?.eval(ctx)`, with `~` routed through [`concat()`].
fn eval_expr(
    templar: &Templar,
    expr: &str,
    ctx: &minijinja::Value,
) -> Result<minijinja::Value, minijinja::Error> {
    let ast = parse_expr(expr)?;
    let mut codegen = CodeGenerator::new("<expression>", expr);
    codegen.compile_expr(&ast);
    let (mut instructions, _) = codegen.finish();
    route_calls(&mut instructions);
    let env = templar.env_for([&instructions]);
    let mut out = String::new();
    let (value, _) = Vm::new(&env).eval(
        &instructions,
        ctx.clone(),
        &BTreeMap::new(),
        &mut make_string_output(&mut out),
        AutoEscape::None,
    )?;
    Ok(value.unwrap_or_default())
}

fn convert_error(err: minijinja::Error) -> TemplateError {
    // An operator given an undefined operand is an undefined read (`nope | int + 1` fails
    // `'nope' is undefined`, measured on ansible-core 2.19.12), worded as any other; minijinja
    // names the operand's kind in its message and has no other way to tell it.
    let undefined_operand = err.kind() == ErrorKind::InvalidOperation
        && err.detail().is_some_and(|d| {
            d.starts_with("tried to use ")
                && (d.contains(" unsupported types undefined and ")
                    || d.ends_with(" and undefined"))
        });
    if undefined_operand {
        return convert_error(minijinja::Error::from(ErrorKind::UndefinedError));
    }
    // A bridged call's failure is worded in full where it is raised, as the reference words it.
    if let Some(said) = bridged::said(&err) {
        return TemplateError(said.to_string());
    }
    let text = err.to_string();
    if err.kind() == ErrorKind::UndefinedError {
        return TemplateError(format!("{UNDEFINED} The error was: {text}"));
    }
    TemplateError(text)
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::testing::templar;
    use serde_json::json;

    fn vars(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    /// A single expression keeps its type, text renders to a string, values template recursively and
    /// keys do not, variables resolve through chains, braces and file names do not change the answer.
    #[test]
    fn values_keep_their_type_and_text_renders_to_a_string() {
        {
            let t = templar();
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
        {
            let t = templar();
            let v = t
                .render_value(
                    &json!({"{{ k }}": ["{{ x }}", {"n": "{{ x + 1 }}"}], "lit": 3}),
                    &vars(json!({"k": "key", "x": 1})),
                )
                .unwrap();
            assert_eq!(v, json!({"{{ k }}": [1, {"n": 2}], "lit": 3}));
        }
        {
            let t = templar();
            let resolved = t
                .resolve_vars_tainted(&vars(
                    json!({"a": "{{ b }}", "b": "{{ c }}!", "c": "x", "bad": "{{ missing }}"}),
                ))
                .0;
            assert_eq!(resolved["a"], json!("x!"));
            assert_eq!(resolved["c"], json!("x"));
            assert_eq!(resolved["bad"], json!("{{ missing }}"));
        }
        {
            let t = templar();
            let none = Map::new();
            assert_eq!(
                t.render("{{ {'a': {'x': 5}} }}", &none),
                Ok(json!({"a": {"x": 5}}))
            );
            assert_eq!(t.render("{{ '}}' }}", &none), Ok(json!("}}")));
            assert!(t.render("{{ 1 }}{{ 2 }}", &none).unwrap().is_string());
            assert!(
                t.render("{{ {'a': 1} }} {{ 2 }}", &none)
                    .unwrap()
                    .is_string()
            );
            assert_eq!(
                t.condition("{{ {'a': {'x': 5}} == {'a': {'x': 5}} }}", &none),
                Ok(true)
            );
        }
        {
            let t = templar();
            for name in ["index.html.j2", "server.xml", "a.htm.jinja2"] {
                let opts = FileRender {
                    name: Some(name.into()),
                    ..FileRender::default()
                };
                assert_eq!(
                    t.render_file("{{ \"<b>\" }} &", &Map::new(), &opts),
                    Ok("<b> &".to_string()),
                    "{name}"
                );
            }
        }
    }

    /// An undefined variable is an error in Ansible's words and stays undefined through every filter
    /// and test, except the ones made to see it; every minijinja builtin goes through the wrapper.
    #[test]
    fn an_undefined_input_stays_undefined_through_every_filter_and_test() {
        {
            let t = templar();
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
        {
            {
                let t = templar();
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
            {
                let t = templar();
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
            {
                let bare = format!("{:?}", Environment::new());
                let ours = format!("{:?}", templar().env());
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

    /// `hostvars` answers the five shapes a playbook reads it with, from a map, the shared view, and a
    /// bare map without a view.
    #[test]
    fn hostvars_answers_alike_from_a_map_a_view_or_a_bare_map() {
        {
            let t = templar();
            let map = hostvars_map();
            for (text, want) in hostvars_cases() {
                assert_eq!(t.render(text, &map).unwrap(), want, "{text}");
            }
        }
        {
            let t = templar();
            let shared = Arc::new(vars(json!({"a": {"y": 1}, "b": {"y": 2}})));
            let empty = Map::new();
            for (text, want) in hostvars_cases() {
                let vars = Vars {
                    map: &empty,
                    hostvars: Some(&shared),
                    shared: None,
                    facts: None,
                    untrusted: None,
                    untrusted_hosts: None,
                };
                assert_eq!(t.render(text, vars).unwrap(), want, "{text}");
            }
        }
        {
            let t = templar();
            assert_eq!(
                t.render("{{ hostvars['a'].y }}", &hostvars_map()).unwrap(),
                json!(1)
            );
        }
    }

    /// The context hands out the shared view itself, builds it once, ranks inventory-wide names over
    /// a fact and lists a shared name once.
    #[test]
    fn the_context_hands_out_the_shared_view_and_ranks_inventory_wide_names() {
        {
            let shared = Arc::new(vars(json!({"a": {"y": 1}, "b": {"y": 2}})));
            let empty = Map::new();
            let (ctx, _) = context_of(Vars {
                map: &empty,
                hostvars: Some(&shared),
                shared: None,
                facts: None,
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
        {
            let t = templar();
            let shared = Arc::new(vars(json!({"groups": {"web": ["h1"]}})));
            let fact = vars(json!({"groups": "a set_fact wrote this", "own": 1}));
            let vars = Vars {
                map: &fact,
                hostvars: None,
                shared: Some(&shared),
                facts: None,
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
        {
            let shared = Arc::new(vars(json!({"groups": {"web": ["h1"]}})));
            let fact = vars(json!({"groups": "a set_fact wrote this", "own": 1}));
            let (ctx, _) = context_of(Vars {
                map: &fact,
                hostvars: None,
                shared: Some(&shared),
                facts: None,
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
    }

    /// `extract` over `hostvars` finds each host's address (the k3s CIDR loop) and taints like a bare
    /// read.
    #[test]
    fn extract_reads_hostvars_by_host_name_and_taints_like_a_bare_read() {
        {
            let t = templar();
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
                    facts: None,
                    untrusted: None,
                    untrusted_hosts: None,
                };
                assert_eq!(t.render(loop_text, v).unwrap(), want, "{server_group}");
            }
        }
        {
            let t = templar();
            let view = Arc::new(vars(json!({"h2": {"x": "{{ 1 + 1 }}"}})));
            let empty = Map::new();
            let hosts = BTreeSet::from(["h2".to_string()]);
            let mut wrong = Vec::new();
            for (untrusted_hosts, want) in [(Some(&hosts), json!("{{ 1 + 1 }}")), (None, json!(2))]
            {
                let v = Vars {
                    map: &empty,
                    hostvars: Some(&view),
                    shared: None,
                    facts: None,
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
    }

    /// Conditions need a boolean and accept stray braces; a delimited one reads an author string
    /// again and never a host one.
    #[test]
    fn conditions_need_a_boolean_and_never_evaluate_a_host_string() {
        {
            let t = templar();
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
        {
            let t = templar();
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
                facts: None,
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

    /// A literal, argument, container, concatenation or arithmetic holding an undefined value is an
    /// undefined read, never `null` or an empty string.
    #[test]
    #[expect(clippy::too_many_lines)]
    fn a_value_holding_an_undefined_one_is_an_undefined_read_in_every_shape() {
        {
            let t = templar();
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
        {
            let t = templar();
            let vars = vars(json!({"hs": ["a", "b"], "hv": {"a": {"ip": "10.0.0.1"}, "b": {}}}));
            for text in [
                "{{ [1, nope] | join(',') }}",
                "{{ [1, nope] | string }}",
                "{{ [1, nope] | map('quote') | join(' ') }}",
                "{{ [1, nope] | quote }}",
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
        {
            let t = templar();
            let none = Map::new();
            for text in [
                "{{ 'x' ~ [1, nope] }}",
                "{{ 'x' ~ nope }}",
                "{{ 'x' ~ {'k': nope} }}",
                "{{ [1, nope] ~ 'x' }}",
                "pre {{ 'x' ~ [1, nope] }} post",
                "{% block b %}{{ 'x' ~ [1, nope] }}{% endblock %}",
                "{{ ('x' ~ [1, nope]) | default('d') }}",
                "{{ ('x' ~ nope) | default('d') }}",
                "{{ 'a' ~ 'b' ~ [1, nope] }}",
            ] {
                let err = t.render(text, &none).expect_err(&format!("leaked: {text}"));
                assert!(err.is_undefined(), "{text}: {err}");
            }
            let err = t
                .condition("'x' ~ [1, nope] == 'x'", &none)
                .expect_err("leaked: when: 'x' ~ [1, nope] == 'x'");
            assert!(err.is_undefined(), "{err}");
            let err = t
                .render_file("{{ 'x' ~ [1, nope] }}", &none, &FileRender::default())
                .expect_err("leaked: template file {{ 'x' ~ [1, nope] }}");
            assert!(err.is_undefined(), "{err}");
            for (text, want) in [
                ("{{ 'a' ~ 'b' }}", json!("ab")),
                ("{{ 'a' ~ 'b' ~ 'c' }}", json!("abc")),
                ("{{ 1 ~ 2 }}", json!("12")),
                ("{{ 'x' ~ [1, 2] }}", json!("x[1, 2]")),
                ("{{ ([1, nope] + [2]) | length }}", json!(3)),
            ] {
                assert_eq!(t.render(text, &none), Ok(want), "{text}");
            }
            assert_eq!(t.condition("'a' ~ 'b' == 'ab'", &none), Ok(true));
            // Variables, not literals: minijinja folds `'a' ~ 'b'` at compile time, with no call.
            let shadow = vars(json!({ "volant::concat": 1, "a": "a", "b": "b" }));
            assert_eq!(t.render("{{ a ~ b }}", &shadow), Ok(json!("ab")));
        }
        {
            let t = templar();
            let none = Map::new();
            for text in [
                "{{ nope | int + 1 }}",
                "{{ 2 * (nope | int) }}",
                "{{ nope | int - 1 }}",
            ] {
                let err = t.render(text, &none).unwrap_err();
                assert!(err.is_undefined(), "{text}: {err}");
                assert_eq!(err, t.render("{{ nope }}", &none).unwrap_err(), "{text}");
            }
            let err = t.render("{{ 'a' - 1 }}", &none).unwrap_err();
            assert!(!err.is_undefined(), "{err}");
        }
    }

    /// The idioms roles write around undefined values, each measured on ansible-core 2.19.12.
    #[test]
    fn the_idioms_around_undefined_values_answer_as_the_reference() {
        let t = templar();
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

    /// A bridge whose helper `start` gives, and the count of its starts.
    fn bridged_templar(
        start: fn() -> anyhow::Result<crate::python::PythonBuilder>,
    ) -> (Templar, Arc<std::sync::atomic::AtomicUsize>) {
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&starts);
        let bridge = Bridge::with_start(Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            start()
        }));
        (
            Templar::with_bridge(PathBuf::from("."), Some(bridge)),
            starts,
        )
    }

    fn no_helper() -> anyhow::Result<crate::python::PythonBuilder> {
        anyhow::bail!("no ansible-core here")
    }

    fn dying_helper() -> anyhow::Result<crate::python::PythonBuilder> {
        crate::python::PythonBuilder::spawn("python3", &["-c", "import sys; sys.exit(3)"], &[])
    }

    /// A stand-in helper answering every call as `argv[1]` says: `not_found`, `failed`, or the
    /// call's own arguments.
    const STAND_IN: &str = r#"
import json, struct, sys
mode = sys.argv[1]
while True:
    head = sys.stdin.buffer.read(4)
    if len(head) < 4:
        break
    request = json.loads(sys.stdin.buffer.read(struct.unpack(">I", head)[0]))
    answer = {"id": request["id"]}
    if mode == "not_found":
        answer["error"] = {"kind": "not_found", "message": "The lookup plugin 'x.y' was not found."}
    elif mode == "failed":
        answer["error"] = {"kind": "plugin", "message": "The filter plugin 'x.y' failed: boom"}
    else:
        answer["value"] = request["call"]["args"]
    raw = json.dumps(answer).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(raw)) + raw)
    sys.stdout.buffer.flush()
"#;

    fn empty_helper() -> anyhow::Result<crate::python::PythonBuilder> {
        crate::python::PythonBuilder::spawn("python3", &["-c", STAND_IN, "not_found"], &[])
    }

    fn failing_helper() -> anyhow::Result<crate::python::PythonBuilder> {
        crate::python::PythonBuilder::spawn("python3", &["-c", STAND_IN, "failed"], &[])
    }

    fn echo_helper() -> anyhow::Result<crate::python::PythonBuilder> {
        crate::python::PythonBuilder::spawn("python3", &["-c", STAND_IN, "echo"], &[])
    }

    /// What the helper answers reaches the template: a value, a plugin's own failure, a name it
    /// does not know.
    #[rstest::rstest]
    #[case::filter_value(echo_helper, "{{ 'a' | x.y('b', k=1) }}", Ok(json!(["a", "b"])))]
    #[case::test_value(echo_helper, "{{ 'a' is x.y }}", Ok(json!(true)))]
    #[case::lookup_value(echo_helper, "{{ lookup('x.y', 't') }}", Ok(json!(["t"])))]
    #[case::query_value(echo_helper, "{{ q('x.y', 't', wantlist=False) }}", Ok(json!(["t"])))]
    #[case::undefined_filter_input(echo_helper, "{{ nope | x.y | default(1) }}", Ok(json!(1)))]
    #[case::plugin_failure(
        failing_helper,
        "{{ 'a' | x.y }}",
        Err("The filter plugin 'x.y' failed: boom")
    )]
    #[case::unknown_test(
        empty_helper,
        "{{ 'a' is x.y }}",
        Err("unknown test: test x.y is unknown (in <expression>:1)")
    )]
    #[case::unknown_lookup(
        empty_helper,
        "{{ lookup('x.y') }}",
        Err("The lookup plugin 'x.y' was not found.")
    )]
    #[case::jinja_object_in_a_lookup(
        echo_helper,
        "{{ lookup('x.y', range) }}",
        Err("lookup 'x.y' was given a Jinja object, which cannot be sent to the Python helper")
    )]
    #[case::jinja_object_in_a_keyword(
        echo_helper,
        "{{ 'a' is x.y(k=range) }}",
        Err("test 'x.y' was given a Jinja object, which cannot be sent to the Python helper")
    )]
    fn what_the_helper_answers_reaches_the_template(
        #[case] helper: fn() -> anyhow::Result<crate::python::PythonBuilder>,
        #[case] text: &str,
        #[case] want: Result<Value, &str>,
    ) {
        let (t, _) = bridged_templar(helper);
        assert_eq!(
            t.render(text, &Map::new()),
            want.map_err(|e| TemplateError(e.into())),
            "{text}"
        );
    }

    #[test]
    fn an_undefined_value_given_to_a_test_is_an_undefined_read() {
        let (t, _) = bridged_templar(echo_helper);
        let err = t.render("{{ nope is x.y }}", &Map::new()).unwrap_err();
        assert!(err.is_undefined(), "{err}");
    }

    /// ansible-core resolves a filter's name before it looks at the value: an undefined value
    /// given to a filter nobody has fails as unknown, not as undefined.
    #[test]
    fn an_undefined_value_given_to_a_missing_filter_fails_as_unknown() {
        let (t, _) = bridged_templar(empty_helper);
        assert_eq!(
            t.render("{{ nope | x.y | default('d') }}", &Map::new()),
            Err(TemplateError(
                "unknown filter: filter x.y is unknown (in <expression>:1)".into()
            ))
        );
    }

    /// A native name never starts the helper; what the engine lacks goes to it, or fails naming it.
    #[rstest::rstest]
    #[case::k3s_ipwrap_stays_native("{{ '::1' | ansible.utils.ipwrap }}", Ok(json!("[::1]")), 0)]
    #[case::qualified_lookup_stays_native("{{ lookup('ansible.builtin.vars', 'x') }}", Ok(json!(1)), 0)]
    #[case::qualified_builtin_stays_native("{{ [1] | ansible.builtin.length }}", Ok(json!(1)), 0)]
    #[case::native_test("{{ 'a' is ansible.builtin.string }}", Ok(json!(true)), 0)]
    #[case::native_lookup("{{ query('vars', 'x') }}", Ok(json!([1])), 0)]
    #[case::unvault_stays_native("{{ 'x' | unvault('s') }}", Ok(json!("x")), 0)]
    #[case::vault_test_stays_native("{{ 'x' is vault_encrypted }}", Ok(json!(false)), 0)]
    #[case::undefined_input_still_resolves_the_name(
        "{{ nope | community.general.dict_kv('k') | default({}) }}",
        Ok(json!({})),
        1
    )]
    #[case::legacy_native_stays_native("{{ [1] | ansible.legacy.length }}", Ok(json!(1)), 0)]
    #[case::filter_named_by_map(
        "{{ ['v'] | map('community.general.dict_kv', 'k') | list }}",
        Err(
            "filter 'community.general.dict_kv' needs the Python helper, which could not start: no ansible-core here"
        ),
        1
    )]
    #[case::jinja_object_is_refused(
        "{% macro m() %}{% endmacro %}{{ m | community.general.dict_kv('k') }}",
        Err(
            "filter 'community.general.dict_kv' was given a Jinja object, which cannot be sent to the Python helper"
        ),
        0
    )]
    #[case::a_filter(
        "{{ 'v' | community.general.dict_kv('k') }}",
        Err(
            "filter 'community.general.dict_kv' needs the Python helper, which could not start: no ansible-core here"
        ),
        1
    )]
    #[case::a_test(
        "{{ 'a' is ansible.builtin.abs }}",
        Err(
            "test 'ansible.builtin.abs' needs the Python helper, which could not start: no ansible-core here"
        ),
        1
    )]
    #[case::a_lookup(
        "{{ q('sequence', 'end=3') }}",
        Err(
            "lookup 'sequence' needs the Python helper, which could not start: no ansible-core here"
        ),
        1
    )]
    fn a_name_the_engine_lacks_goes_to_the_python_helper(
        #[case] text: &str,
        #[case] want: Result<Value, &str>,
        #[case] starts: usize,
    ) {
        let (t, started) = bridged_templar(no_helper);
        let vars = vars(json!({"x": 1}));
        assert_eq!(
            t.render(text, &vars),
            want.map_err(|e| TemplateError(e.into())),
            "{text}"
        );
        assert_eq!(started.load(Ordering::SeqCst), starts, "{text}");
    }

    #[test]
    fn a_helper_that_dies_fails_the_render_and_every_later_one() {
        let (t, _) = bridged_templar(dying_helper);
        let text = "{{ 'v' | community.general.dict_kv('k') }}";
        let first = t.render(text, &Map::new()).unwrap_err();
        assert!(
            first.0.starts_with(
                "filter 'community.general.dict_kv' needs the Python helper, which failed: the Python helper exited"
            ),
            "{first}"
        );
        assert_eq!(t.render(text, &Map::new()), Err(first));
    }

    /// A `!vault` value is read when a task uses it and is text from then on; one that cannot be
    /// read raises only where something renders it.
    #[test]
    fn a_vaulted_value_is_text_when_read_and_raises_only_when_used() {
        let cipher = crate::vault::encrypt(b"{{ x }} and more", b"pw", None).unwrap();
        let map = vars(json!({"x": "hello", "sv": crate::yaml::vault_data(&cipher)}));
        let t = templar();
        crate::vault::install(crate::vault::Secrets::new(vec![b"pw".to_vec()]));
        let (resolved, untrusted) = t.resolve_vars_tainted(&map);
        assert_eq!(resolved["sv"], json!("{{ x }} and more"));
        assert!(untrusted.contains("sv"));
        crate::vault::install(crate::vault::Secrets::new(vec![b"other".to_vec()]));
        let (resolved, _) = t.resolve_vars_tainted(&map);
        let err = t.render("{{ sv }}", &resolved).unwrap_err();
        assert_eq!(
            err.0,
            "Attempt to use undecryptable variable: Decryption failed (no vault secrets were found that could decrypt)."
        );
    }

    /// Inside text a container prints as Python's `repr` and a whole `none` prints nothing; an
    /// undefined parent stays undefined until `default` decides; a final newline is kept.
    #[test]
    fn text_prints_containers_none_and_chained_undefined_as_the_reference_does() {
        let t = templar();
        let v = vars(json!({"l": ["a", null], "n": null, "d": {"k": true}, "nl": "x"}));
        for (text, want) in [
            (
                "{{ l }} {{ d }}[{{ n }}]",
                json!("['a', None] {'k': True}[]"),
            ),
            ("{{ n }}", json!(null)),
            ("{{ nope.a.b | default('z') }}", json!("z")),
            ("{{ d.no.sub | default('z') }}", json!("z")),
            ("{{ d['no']['sub'] is defined }}", json!(false)),
            ("{{ nl }}\n", json!("x\n")),
        ] {
            assert_eq!(t.render(text, &v).unwrap(), want, "{text}");
        }
        assert!(t.render("{{ nope.a }}", &v).is_err());
        assert!(t.render("x {{ d.no.sub }}", &v).is_err());
    }
}
