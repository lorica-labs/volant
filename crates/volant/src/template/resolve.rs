// SPDX-License-Identifier: GPL-3.0-or-later
//! Resolving the variables that hold templates: the whole map ahead of a task, and one variable
//! when a template reads it.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use serde_json::{Map, Value};

use super::context::{Vars, context_tracking, is_literal};
use super::templar::Templar;
use super::vm::{holds_template, unwrap_unsafe};
use super::{Tainted, TemplateError};

impl Templar {
    /// What a template reads a variable as when the variable holds a template: a string renders
    /// like any other, and a failure comes back to the reader. A list or mapping renders string
    /// by string and leaves one that fails as written, so a member nothing reads raises nothing.
    pub(super) fn render_read(
        &self,
        value: &Value,
        ctx: &minijinja::Value,
        tainted: &Tainted,
        any: &mut bool,
    ) -> Result<Value, TemplateError> {
        match value {
            Value::String(text) => {
                let out = self.render_in(text, ctx, tainted)?;
                *any |= tainted.load(Ordering::Relaxed);
                Ok(out)
            }
            other => Ok(self.render_value_lenient(other, ctx, tainted, any)),
        }
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
            let (ctx, tainted) = context_tracking(
                self,
                Vars {
                    map: &current,
                    hostvars: vars.hostvars,
                    shared: vars.shared,
                    facts: vars.facts,
                    untrusted: Some(&untrusted),
                    untrusted_hosts: vars.untrusted_hosts,
                    base: vars.base,
                },
                vars.untrusted.is_some(),
            );
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
            Value::String(s) if Self::is_stored_template(s) => {
                let out = self
                    .render_in(s, ctx, tainted)
                    .unwrap_or_else(|_| value.clone());
                *any |= tainted.load(Ordering::Relaxed) || is_literal(ctx);
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
}
