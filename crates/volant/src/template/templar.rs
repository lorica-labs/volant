// SPDX-License-Identifier: GPL-3.0-or-later
//! `Templar`: renders templates, expressions and conditionals against a set of variables.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use minijinja::machinery::{Instruction, Instructions};
use minijinja::{Environment, ErrorKind};
use serde_json::Value;

use crate::bridge::{Bridge, PluginKind};

use super::bridged;
use super::context::{Vars, begin, context_of, take_failure};
use super::env::{environment, holds_undefined, truthy, type_name};
use super::missing::{named, omitting, without_undefined};
use super::vm::{TemplateReads, convert_error, eval_expr, single_expression, template_reads};
use super::{Tainted, TemplateError, as_expression, inline_conditional};

/// A handle on the environment and the bridge, cheap to clone: a render's context holds one so
/// that a variable whose value is a template can be rendered when a template reads it.
#[derive(Clone)]
pub struct Templar {
    inner: Arc<Shared>,
}

struct Shared {
    env: RwLock<Bridged>,
    bridge: Option<Arc<Bridge>>,
    /// What each template text read, found once: a variable's text is analyzed on every task.
    reads: Mutex<HashMap<String, Option<Arc<TemplateReads>>>>,
}

/// Entries kept before the table starts over. Texts are the playbook's own variables, so a run
/// stays far below it.
const MAX_READS: usize = 8192;

impl std::fmt::Debug for Templar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Templar")
    }
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
        self.inner.bridge.as_ref()
    }

    /// [`Templar::new`], sending what the engine lacks to `bridge`: a filter or test the engine
    /// has no native implementation of, a lookup it does not run. A native name never reaches it.
    pub fn with_bridge(base_dir: PathBuf, bridge: Option<Arc<Bridge>>) -> Self {
        Self {
            inner: Arc::new(Shared {
                env: RwLock::new(Bridged {
                    env: Arc::new(environment(base_dir, bridge.as_ref())),
                    seen: HashSet::new(),
                }),
                bridge,
                reads: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The environment as it stands.
    pub(crate) fn env(&self) -> Arc<Environment<'static>> {
        Arc::clone(
            &self
                .inner
                .env
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .env,
        )
    }

    /// The environment to run `code` in: [`Templar::env`], plus a call to the bridge under each
    /// filter and test name `code` uses that the engine has no native implementation of.
    pub(super) fn env_for<'a, 's: 'a>(
        &self,
        code: impl IntoIterator<Item = &'a Instructions<'s>>,
    ) -> Arc<Environment<'static>> {
        let Some(bridge) = &self.inner.bridge else {
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
            let current = self
                .inner
                .env
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            if used.iter().all(|name| current.seen.contains(name)) {
                return Arc::clone(&current.env);
            }
        }
        let mut current = self
            .inner
            .env
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let Bridged { env, seen } = &mut *current;
        for (kind, name) in used {
            if seen.contains(&(kind, name.clone())) {
                continue;
            }
            if bridged::sendable_name(kind, &name) && !bridged::native(env, bridge, kind, &name) {
                bridged::register(Arc::make_mut(env), bridge, kind, &name);
            }
            seen.insert((kind, name));
        }
        Arc::clone(env)
    }

    /// Whether the template `text` may read one of `names`: `true` when that cannot be told.
    /// For the profile, so the cache's own counters leave it out.
    pub fn may_read(&self, text: &str, names: &[&str]) -> bool {
        self.reads(text).is_none_or(|reads| {
            reads.dynamic || names.iter().any(|name| reads.names.contains(*name))
        })
    }

    fn reads(&self, text: &str) -> Option<Arc<TemplateReads>> {
        let mut table = self
            .inner
            .reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(known) = table.get(text) {
            return known.clone();
        }
        let found = template_reads(text).map(Arc::new);
        if table.len() >= MAX_READS {
            table.clear();
        }
        table.insert(text.to_string(), found.clone());
        found
    }

    /// Renders a string. Exactly one `{{ expression }}` gives the expression's value with its
    /// type; text around or between expressions gives a string.
    pub fn render<'a>(
        &self,
        text: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<Value, TemplateError> {
        let (ctx, tainted) = context_of(self, vars.into());
        self.render_in(text, &ctx, &tainted)
    }

    pub(super) fn render_in(
        &self,
        text: &str,
        ctx: &minijinja::Value,
        tainted: &Tainted,
    ) -> Result<Value, TemplateError> {
        super::unknown::through(|| self.render_known(text, ctx, tainted))
    }

    /// A render happens once: a variable holding a trusted template is rendered when it is read,
    /// so what comes out is a result, and no marker in it is a template.
    fn render_known(
        &self,
        text: &str,
        ctx: &minijinja::Value,
        tainted: &Tainted,
    ) -> Result<Value, TemplateError> {
        // Per string, not per context: `render_value_in` walks a whole structure through one
        // context, and a tainted field must not make the next field data too.
        tainted.store(false, Ordering::Relaxed);
        begin(ctx);
        self.render_once(text, ctx)
    }

    /// Templates every string inside a value. Mapping keys are left alone.
    pub fn render_value<'a>(
        &self,
        value: &Value,
        vars: impl Into<Vars<'a>>,
    ) -> Result<Value, TemplateError> {
        Ok(self.render_value_tainted(value, vars)?.0)
    }

    /// `render_value`, saying as well whether a string of it read a name from a managed host or a
    /// file at run time.
    pub fn render_value_tainted<'a>(
        &self,
        value: &Value,
        vars: impl Into<Vars<'a>>,
    ) -> Result<(Value, bool), TemplateError> {
        let (ctx, tainted) = context_of(self, vars.into());
        let mut any = false;
        let (out, _) = self.render_value_in(value, &ctx, &tainted, &mut any, None)?;
        Ok((out, any))
    }

    /// `render_value_tainted`, with the tags of the value in place of the answer.
    pub fn render_value_tagged<'a>(
        &self,
        value: &Value,
        vars: impl Into<Vars<'a>>,
    ) -> Result<(Value, bool, Option<crate::tags::TagTree>), TemplateError> {
        let (ctx, tainted) = context_of(self, vars.into());
        let mut any = false;
        let tagging = Some(super::render_map::Tagging { origin: None });
        let (out, tree) = self.render_value_in(value, &ctx, &tainted, &mut any, tagging)?;
        Ok((out, any, tree))
    }

    /// Evaluates one expression and returns its value with its type.
    pub fn evaluate<'a>(
        &self,
        expr: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<Value, TemplateError> {
        self.evaluate_in(expr, &context_of(self, vars.into()).0)
    }

    /// `evaluate`, and whether the value is data: a lookup read a file, a command or the
    /// environment, or the expression read a name that came from a managed host.
    pub fn evaluate_tainted<'a>(
        &self,
        expr: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<(Value, bool), TemplateError> {
        let (ctx, tainted) = context_of(self, vars.into());
        let value = self.evaluate_in(expr, &ctx)?;
        Ok((value, tainted.load(Ordering::Relaxed)))
    }

    /// `evaluate`, with the tags of the value: a string they trust is one a read left as written.
    pub fn evaluate_tagged<'a>(
        &self,
        expr: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<(Value, Option<crate::tags::TagTree>), TemplateError> {
        let (ctx, _) = context_of(self, vars.into());
        let value = self.evaluate_in(expr, &ctx)?;
        Ok((value, super::tagged::take_result(&ctx)))
    }

    pub(super) fn evaluate_in(
        &self,
        expr: &str,
        ctx: &minijinja::Value,
    ) -> Result<Value, TemplateError> {
        super::unknown::through(|| self.evaluate_known(expr, ctx))
    }

    fn evaluate_known(&self, expr: &str, ctx: &minijinja::Value) -> Result<Value, TemplateError> {
        let omit = omitting();
        let value = eval_expr(self, expr, ctx).map_err(|e| {
            super::jinja_syntax(&e, expr).map_or_else(|| convert_error(e), TemplateError)
        });
        // A variable the expression read failed to render: that is the error, whatever the
        // undefined value it was read as did to the expression.
        if let Some(error) = take_failure(ctx) {
            return Err(error);
        }
        let mut value = value.map_err(|e| super::order::tagged(named(e, expr, ctx), expr, ctx))?;
        if omit && !value.is_undefined() {
            value = without_undefined(&value);
        }
        // Strict mode only raises on operations that force an undefined value (printing,
        // comparing, ...); an attribute lookup that never gets used stays a lazy Undefined, alone
        // or inside a list or mapping, that serde_json would otherwise turn into `null`. Force
        // the same error here.
        if holds_undefined(&value) {
            return Err(named(
                convert_error(minijinja::Error::from(ErrorKind::UndefinedError)),
                expr,
                ctx,
            ));
        }
        super::tagged::set_result(ctx, Some(&value));
        super::pymethods::bytes::to_json(&value)
            .map_err(|e| TemplateError(super::pymethods::bytes::result_error(&e)))
    }

    /// A `when` clause. Ansible accepts `{{ }}` around it with a warning; the result must be a
    /// boolean, anything else is an error in the reference release.
    ///
    /// A delimited condition that renders to a string is evaluated again as a condition when the
    /// string is trusted as a template, the way the reference treats a string an author wrote
    /// (`healthy: "true"`, `cond: "1 == 1"`). Any other string is data, refused here: compiling it
    /// is how a host would run code on the controller.
    pub fn condition<'a>(
        &self,
        expr: &str,
        vars: impl Into<Vars<'a>>,
    ) -> Result<bool, TemplateError> {
        let (ctx, _) = context_of(self, vars.into());
        let delimited = single_expression(expr);
        // A condition written in braces is a template: its syntax errors are a template's.
        let mut value = match delimited {
            Some(inner) => super::as_template(expr, inner, || self.evaluate_in(inner, &ctx))?,
            None => self
                .evaluate_in(&inline_conditional(expr), &ctx)
                .map_err(as_expression)?,
        };
        if delimited.is_some()
            && let Value::String(text) = &value
        {
            let trusted = super::tagged::take_result(&ctx)
                .is_some_and(|tree| crate::tags::is_trusted(tree.own()));
            if !trusted {
                return Err(TemplateError(super::UNTRUSTED.into()));
            }
            value = self.evaluate_in(single_expression(text).unwrap_or(text.trim()), &ctx)?;
        }
        // Read by the replay, which takes the recorded branch: see `crate::shadow::replay`.
        if let Value::String(text) = &value
            && let Some(reason) = super::unknown::reason_in(text)
        {
            return Err(TemplateError(super::unknown::marker(reason)));
        }
        match value {
            Value::Bool(b) => Ok(b),
            other if super::allow_broken_conditionals() => {
                let held = truthy(&other);
                crate::playbook::print_deprecation(&format!(
                    "Conditional result ({}) was derived from value of type '{}'. Conditionals must have a boolean result. This feature will be removed from ansible-core version 2.23.\n\nBroken conditionals are currently allowed because the `ALLOW_BROKEN_CONDITIONALS` configuration option is enabled.",
                    if held { "True" } else { "False" },
                    type_name(&other),
                ));
                Ok(held)
            }
            other => Err(TemplateError(format!(
                "Conditional result was {other} of type {}, which evaluates to {}. Conditionals must have a boolean result.",
                type_name(&other),
                if truthy(&other) { "True" } else { "False" }
            ))),
        }
    }
}
