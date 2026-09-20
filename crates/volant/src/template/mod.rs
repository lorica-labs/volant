// SPDX-License-Identifier: GPL-3.0-or-later
//! Jinja2 templating the way ansible-core 2.19 does it: strict about undefined variables, and a
//! template that is one expression yields that expression's value, not its text.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use minijinja::value::{Enumerator, Object};
use minijinja::{Environment, ErrorKind, UndefinedBehavior};
use serde_json::{Map, Value};

mod filters;

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
        // comparing, ...); an attribute lookup that never gets used stays a lazy Undefined that
        // serde_json would otherwise turn into `null`. Force the same error here.
        if value.is_undefined() {
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
    pub fn condition<'a>(
        &self,
        expr: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<bool, TemplateError> {
        let expr = single_expression(expr).unwrap_or(expr.trim());
        match self.evaluate(expr, vars)? {
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
mod tests {
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
}
