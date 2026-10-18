// SPDX-License-Identifier: GPL-3.0-or-later
//! `Templar`: renders templates, expressions and conditionals against a set of variables.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, RwLock};

use minijinja::machinery::{Instruction, Instructions, WhitespaceConfig};
use minijinja::{Environment, ErrorKind};
use serde_json::{Map, Value};

use crate::bridge::{Bridge, PluginKind};

use super::bridged;
use super::context::{Vars, context_of};
use super::env::{environment, holds_undefined, truthy, type_name};
use super::filters;
use super::vm::{
    convert_error, eval_expr, holds_template, render_str, single_expression, unwrap_unsafe,
};
use super::{RESOLVING, Tainted, TemplateError};

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
    pub(super) fn env_for<'a, 's: 'a>(
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

    pub(super) fn render_in(
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
