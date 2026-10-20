// SPDX-License-Identifier: GPL-3.0-or-later
//! How a render reads one name of the host's variables: a value that holds a template of the
//! playbook is rendered when a template reads it, and what a host wrote is left as data.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::super::templar::Templar;
use super::super::vm::holds_template;
use super::super::{HOSTVARS_KEYS, RECURSION, TemplateError};
use super::{ALLITEMS_KEY, Context, Hostvars, LOOP_KEY, LoopView, Memo};

/// Names of the map that hold the inventory's own names: the host's name, its short name and its
/// groups. Together with the shared layer they are what a name added to the inventory during the
/// run reaches a template through. A caller that does not track which names hold host text cannot
/// tell a read of them from a read of host text, so a variable that reads one is left as written.
/// `hostvars` is not among them: a variable of another host may hold a template of its own,
/// which a read renders.
const INVENTORY_NAMES: [&str; 3] = [
    "inventory_hostname",
    "inventory_hostname_short",
    "group_names",
];

/// Whether `key` is bound by the engine for its own use and never written by a playbook: the
/// internal bindings of a render. A read of such a name gives the value as bound.
fn engine_owned(key: &str) -> bool {
    key.starts_with("__volant_") || key.contains("::")
}

/// Names being resolved at once before a read is called a loop. The reference stops at Python's
/// recursion limit, and a chain this long is never an intended one.
const MAX_RESOLVING: usize = 64;

/// In front of a host's name among the names of `Deferred`: no variable has a name like that.
const HOST_PREFIX: &str = "hostvars/";

/// The reads of a render that handed a template out unrendered: a variable the render does not
/// resolve, or a host whose variables hold one. A result with a template marker is rendered again
/// only when such a read gave it the marker: a marker the pass built from names and text that
/// hold none (`'{{ ' ~ name`, `replace('a', '{{ x }}')`) is text, as the reference leaves it.
#[derive(Debug, Default)]
pub(super) struct Deferred {
    count: AtomicUsize,
    /// The names that did, so that a read answered from the memo counts as the first one did. A
    /// host's name has `HOST_PREFIX` in front.
    names: Mutex<BTreeSet<String>>,
    any: AtomicBool,
}

impl Deferred {
    pub(super) fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    fn mark(&self, name: String) {
        self.names.lock().expect("template deferred").insert(name);
        self.any.store(true, Ordering::Relaxed);
    }

    /// A read of the variable `key` that handed out its template.
    fn note(&self, key: &str) {
        self.mark(key.to_string());
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// A host whose variables `entry` hold a template: each read of it counts.
    pub(super) fn mark_host(&self, host: &str, entry: &Value) {
        if holds_template(entry) {
            self.mark(format!("{HOST_PREFIX}{host}"));
        }
    }

    /// A read of a name that already handed a template out counts again, whatever answers it.
    pub(super) fn again(&self, key: &str) {
        if self.any.load(Ordering::Relaxed) && self.has(key) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn again_host(&self, host: &str) {
        if self.any.load(Ordering::Relaxed) && self.has(&format!("{HOST_PREFIX}{host}")) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn has(&self, name: &str) -> bool {
        self.names.lock().expect("template deferred").contains(name)
    }
}

impl Context {
    /// The untrusted names a render nested in this one starts from. Such a render tracks host
    /// text, so when this one does not, the inventory's names and the shared lists are data there.
    pub(in crate::template) fn untrusted_for_nested(&self) -> BTreeSet<String> {
        let mut names = self.untrusted.clone();
        if !self.tracked {
            names.extend(INVENTORY_NAMES.map(String::from));
            names.extend(self.shared.iter().flat_map(|s| s.keys().cloned()));
        }
        names
    }

    fn fail(&self, error: TemplateError) {
        self.failure
            .lock()
            .expect("template failure")
            .get_or_insert(error);
        self.failed.store(true, Ordering::Relaxed);
    }

    /// What a variable holding a template reads as: the value the template renders to, with the
    /// context the reading template runs in, as the reference does when it reads a variable. The
    /// name was not a managed host's when it got here, so its text is the playbook's.
    ///
    /// Not stored when the render did not finish, so a later read tries again.
    fn resolve(self: &Arc<Self>, key: &str, raw: &Value) -> (minijinja::Value, bool) {
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
        let deferred = self.deferred.count();
        let mut from_host = false;
        let rendered = self
            .templar
            .render_read(raw, &ctx, &self.tainted, &mut from_host);
        from_host |= self.tainted.load(Ordering::Relaxed);
        self.tainted.store(before || from_host, Ordering::Relaxed);
        self.resolving.lock().expect("template resolving").pop();
        // What the variable's own render left unrendered is left for the reader too.
        if self.deferred.count() != deferred {
            self.deferred.mark(key.to_string());
        }
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
            Ok(value) => (crate::template::bigint::to_template(&value), true),
            // A variable that reads a name with no value has none either, which is what
            // `default` and `is defined` ask.
            Err(error) if error.is_undefined() => (minijinja::Value::UNDEFINED, false),
            Err(error) => {
                self.fail(error);
                (minijinja::Value::UNDEFINED, false)
            }
        }
    }

    /// Whether rendering `key` reads nothing but what the playbook wrote: every name its text
    /// looks up is an author-written variable, itself read the same way, or a name a managed
    /// host sent, whose trust is tracked. The inventory's names and the shared lists are tracked
    /// too when the caller gave its set of untrusted names: one that holds host text is in it,
    /// and a read of it taints the render. Without that set, and for any lookup by text, what
    /// they hold can come from a host in a form no tracking sees, so a variable that reads one
    /// is left as written. The rule holds for a variable that reads one that reads one, at any
    /// depth.
    fn reads_author_text_only(&self, key: &str, seen: &mut Vec<String>) -> bool {
        if self.untrusted.contains(key) {
            return true;
        }
        if engine_owned(key)
            || (!self.tracked
                && (INVENTORY_NAMES.contains(&key)
                    || self.shared.as_ref().is_some_and(|s| s.contains_key(key))))
        {
            return false;
        }
        let Some(value) = self.vars.get(key) else {
            return true;
        };
        if !holds_template(value) || seen.iter().any(|name| name == key) {
            return true;
        }
        if let Some(known) = self.author_only.lock().expect("template author").get(key) {
            return *known;
        }
        seen.push(key.to_string());
        let mut texts = Vec::new();
        collect_templates(value, &mut texts);
        let ok = texts.iter().all(|text| {
            self.templar.reads_of(text).is_some_and(|reads| {
                !reads.dynamic
                    && reads
                        .names
                        .iter()
                        .all(|n| self.reads_author_text_only(n, seen))
            })
        });
        seen.pop();
        self.author_only
            .lock()
            .expect("template author")
            .insert(key.to_string(), ok);
        ok
    }

    pub(super) fn read(
        self: &Arc<Self>,
        key: &str,
        untrusted: bool,
    ) -> Option<(minijinja::Value, bool)> {
        // A map that carries a `hostvars` key of its own - a fixture, a recorded scope -
        // keeps being read from the map when no view came with it, so the two sources never
        // disagree about which one answers.
        if key == "hostvars"
            && let Some(hostvars) = &self.hostvars
        {
            let view = minijinja::Value::from_object(Hostvars {
                hosts: Arc::clone(hostvars),
                listed: self.shared.clone(),
                untrusted_hosts: self.untrusted_hosts.clone(),
                keys_untrusted: self.untrusted.contains(HOSTVARS_KEYS),
                tainted: Arc::clone(&self.tainted),
                deferred: Arc::clone(&self.deferred),
                memo: Memo::default(),
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
            if !untrusted && !engine_owned(key) && holds_template(value) {
                if self.reads_author_text_only(key, &mut Vec::new()) {
                    return Some(self.resolve(key, value));
                }
                self.deferred.note(key);
            }
            return Some((crate::template::bigint::to_template(value), true));
        }
        // The variables the items of a loop share hold no template that a render could change:
        // `HostVars::shared_by_items` keeps in `map` every value that has one, except the text of
        // a managed host, which is data. So a read of this layer has nothing to resolve.
        let value = self.base.as_ref()?.get(key)?;
        Some((crate::template::bigint::to_template(value), true))
    }
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

/// The strings of `value` that hold a template.
fn collect_templates<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) if Templar::is_stored_template(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|v| collect_templates(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_templates(v, out)),
        _ => {}
    }
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
