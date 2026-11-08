// SPDX-License-Identifier: GPL-3.0-or-later
//! How a render reads one name of the host's variables: a string trusted as a template is rendered
//! when a template reads it, and any other string is its text.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::Value;

use super::super::tagged::to_template_tagged;
use super::super::vm::holds_trusted_template;
use super::super::{HOSTVARS_KEYS, RECURSION, TemplateError};
use super::{ALLITEMS_KEY, Context, Hostvars, LOOP_KEY, LoopView, Memo};
use crate::tags::TagTree;

/// Whether `key` is bound by the engine for its own use and never written by a playbook: the
/// internal bindings of a render. A read of such a name gives the value as bound.
fn engine_owned(key: &str) -> bool {
    key.starts_with("__volant_") || key.contains("::")
}

/// Names being resolved at once before a read is called a loop. The reference stops at Python's
/// recursion limit, and a chain this long is never an intended one.
const MAX_RESOLVING: usize = 64;

/// ansible-core's private switch, which the integration tests set to see an untrusted template
/// met in a render: measured on 2.19.12, `warning` says so, once per text.
fn warn_untrusted(value: &Value) {
    if let Value::String(text) = value
        && super::super::Templar::is_template(text)
        && std::env::var("_ANSIBLE_TEMPLAR_UNTRUSTED_TEMPLATE_BEHAVIOR").as_deref() == Ok("warning")
    {
        crate::render::warn_once(&format!(
            "[WARNING]: {}\n\n{text}\n\n{}\n\n",
            super::super::UNTRUSTED,
            super::super::UNTRUSTED_HELP,
        ));
    }
}

impl Context {
    /// What the template of `key` failed with when it read a name with no value, and where it
    /// was written.
    pub(in crate::template) fn undefined_in(
        &self,
        key: &str,
    ) -> Option<(String, Option<crate::playbook::Origin>)> {
        self.undefined_in
            .lock()
            .expect("template undefined")
            .get(key)
            .cloned()
    }

    fn fail(&self, error: TemplateError) {
        self.failure
            .lock()
            .expect("template failure")
            .get_or_insert(error);
        self.failed.store(true, Ordering::Relaxed);
    }

    /// Whether a read of `key` reads what a managed host sent, looking in the layers in the order
    /// [`Context::read`] does.
    pub(in crate::template) fn reads_host_text(&self, key: &str) -> bool {
        let listed = |name: &str| self.data_lists.as_ref().is_some_and(|d| d.contains(name));
        if key == "hostvars" {
            let named = if self.hostvars.is_some() {
                "hostvars"
            } else {
                HOSTVARS_KEYS
            };
            if listed(named) {
                return true;
            }
        }
        if self.shared.as_ref().is_some_and(|s| s.contains_key(key)) {
            return listed(key);
        }
        // An `ansible_facts` left empty is what a host with no facts answers: nothing it sent.
        if let Some(fact) = self.facts.as_ref().and_then(|f| f.get(key)) {
            return fact.as_object().is_none_or(|map| !map.is_empty());
        }
        if self.vars.contains_key(key) {
            return self.tree_of(key).is_some_and(from_host);
        }
        self.base
            .as_ref()
            .and_then(|base| base.tree(key))
            .is_some_and(from_host)
    }

    fn tree_of(&self, key: &str) -> Option<&TagTree> {
        self.tags.as_ref()?.get(key)
    }

    /// What a variable holding a template reads as: the value the template renders to, with the
    /// context the reading template runs in, as the reference does when it reads a variable.
    ///
    /// Not stored when the render did not finish, so a later read tries again.
    fn resolve(
        self: &Arc<Self>,
        key: &str,
        raw: &Value,
        tree: Option<&TagTree>,
    ) -> (minijinja::Value, bool) {
        {
            let mut stack = self.resolving.lock().expect("template resolving");
            if stack.len() >= MAX_RESOLVING || stack.iter().any(|name| name == key) {
                drop(stack);
                self.fail(TemplateError(RECURSION.into()));
                return (minijinja::Value::UNDEFINED, false);
            }
            stack.push(key.to_string());
        }
        let ctx = minijinja::Value::from_dyn_object(Arc::clone(self));
        // A failure the render under way already met stays its own: the nested render reports
        // the failures of its variable, and takes only those.
        let earlier = self.failure.lock().expect("template failure").take();
        let earlier_failed = self.failed.swap(false, Ordering::Relaxed);
        // The render under way keeps what it has read so far; the nested one starts clean so that
        // it can tell whether this variable's own text came from a host.
        let before = self.tainted.swap(false, Ordering::Relaxed);
        let mut from_host = false;
        let rendered = self
            .templar
            .render_read(raw, tree, &ctx, &self.tainted, &mut from_host);
        from_host |= self.tainted.load(Ordering::Relaxed);
        self.tainted.store(before || from_host, Ordering::Relaxed);
        self.resolving.lock().expect("template resolving").pop();
        if earlier_failed {
            *self.failure.lock().expect("template failure") = earlier;
            self.failed.store(true, Ordering::Relaxed);
        }
        if from_host {
            self.soiled
                .lock()
                .expect("template soiled")
                .insert(key.to_string());
            self.any_soiled.store(true, Ordering::Relaxed);
        }
        match rendered {
            Ok((value, tree)) => (self.registry.keeping(&value, tree.as_ref()), true),
            // A variable that reads a name with no value has none either, which is what
            // `default` and `is defined` ask.
            Err(error) if error.is_undefined() => {
                let written = raw
                    .as_str()
                    .and_then(|text| crate::template::written::written_at(key, text));
                self.undefined_in
                    .lock()
                    .expect("template undefined")
                    .insert(key.to_string(), (error.0, written));
                (minijinja::Value::UNDEFINED, false)
            }
            Err(error) => {
                self.fail(error);
                (minijinja::Value::UNDEFINED, false)
            }
        }
    }

    pub(super) fn read(self: &Arc<Self>, key: &str) -> Option<(minijinja::Value, bool)> {
        // A map that carries a `hostvars` key of its own - a fixture, a recorded scope -
        // keeps being read from the map when no view came with it, so the two sources never
        // disagree about which one answers.
        if key == "hostvars"
            && let Some(hostvars) = &self.hostvars
        {
            let view = minijinja::Value::from_object(Hostvars {
                hosts: Arc::clone(hostvars),
                listed: self.shared.clone(),
                data_lists: self.data_lists.clone(),
                tainted: Arc::clone(&self.tainted),
                memo: Memo::default(),
                templar: self.templar.clone(),
            });
            return Some((view, true));
        }
        // Before every layer: a magic variable beats a variable of the same name, which a play
        // or `-e` can set. This one is answered only when a template reads it.
        if key == PLAYBOOK_PYTHON {
            return playbook_python();
        }
        // Before the host's own map, never after: these are the inventory-wide values, and
        // they used to be merged into that map last, so they beat a fact of the same name.
        // A loop variable or a registered name that collides with one of them is not here
        // to be found: `HostVars::insert` drops it from the host's shared map as it writes.
        if let Some(shared) = &self.shared
            && let Some(value) = shared.get(key)
        {
            return Some((crate::template::bigint::to_template(value), true));
        }
        // Between the two: the facts that answer, the ones no layer of the map outranks.
        if let Some(facts) = &self.facts
            && let Some(value) = facts.get(key)
        {
            return Some((crate::template::bigint::to_template(value), true));
        }
        if key == LOOP_KEY
            && let (Some(Value::Object(fields)), Some(base)) = (self.vars.get(key), &self.base)
            && base.contains_key(ALLITEMS_KEY)
        {
            let view = minijinja::Value::from_object(LoopView {
                fields: fields.clone(),
                base: Arc::clone(base),
            });
            return Some((view, true));
        }
        if let Some(value) = self.vars.get(key) {
            let tree = self.tree_of(key);
            if !engine_owned(key) && holds_trusted_template(value, tree) {
                return Some(self.resolve(key, value, tree));
            }
            warn_untrusted(value);
            let out = to_template_tagged(value, key, self.tags.as_ref(), &self.registry);
            return Some((out, true));
        }
        // The variables the items of a loop share hold no template that a render could change:
        // `HostVars::shared_by_items` keeps in `map` every value that has one. So a read of this
        // layer has nothing to resolve.
        let base = self.base.as_ref()?;
        let value = base.get(key)?;
        warn_untrusted(value);
        let out = to_template_tagged(value, key, Some(base.tags()), &self.registry);
        Some((out, true))
    }
}

fn from_host(tree: &TagTree) -> bool {
    crate::tags::is_from_host(tree.own())
}

/// The magic variable that names the interpreter running the controller's Python code.
pub(super) const PLAYBOOK_PYTHON: &str = "ansible_playbook_python";

/// A host's entry in `hostvars` with the magic variable set: it beats the host's own value of the
/// name, here as in a task, and every host has it. The lookup is made once per process.
pub(super) fn with_playbook_python(mut entry: Value) -> Value {
    if let Some(map) = entry.as_object_mut() {
        match crate::runtime::resolve::controller_python() {
            Some(python) => map.insert(PLAYBOOK_PYTHON.to_string(), python.into()),
            None => map.remove(PLAYBOOK_PYTHON),
        };
    }
    entry
}

/// `ansible_playbook_python`, the reference's `sys.executable`: the controller's own Python.
/// Looking it up can start a Python probe, so it is answered here, when a template reads it,
/// and never stored with the other magic variables. Undefined when no interpreter resolves.
fn playbook_python() -> Option<(minijinja::Value, bool)> {
    let python = crate::runtime::resolve::controller_python()?;
    Some((minijinja::Value::from(python), true))
}

/// `read`, a read that may go on without the value: a failure it raises is forgotten.
pub(in crate::template) fn forgiving<T>(ctx: &minijinja::Value, read: impl FnOnce() -> T) -> T {
    let earlier = ctx
        .downcast_object_ref::<Context>()
        .is_some_and(|ctx| ctx.failed.load(Ordering::Relaxed));
    let out = read();
    if !earlier {
        take_failure(ctx);
    }
    out
}

/// The error a nested render raised that its reader could not take, if one did, and clears it. A
/// read cannot raise from inside the engine: it answers undefined, and the render that made the
/// read reports the cause in place of whatever that undefined value did to it.
pub(in crate::template) fn take_failure(ctx: &minijinja::Value) -> Option<TemplateError> {
    let ctx = ctx.downcast_object_ref::<Context>()?;
    if !ctx.failed.swap(false, Ordering::Relaxed) {
        return None;
    }
    ctx.failure.lock().expect("template failure").take()
}
