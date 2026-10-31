// SPDX-License-Identifier: GPL-3.0-or-later
//! The facts a looped `set_fact` leaves for the items after it.
//!
//! ansible-core's `TaskExecutor._run_loop` lays what each item's result returned in
//! `ansible_facts` over the variables of the next item. Measured on ansible-core 2.19.12:
//! `set_fact: {x: "{{ x + [item] }}"}` over `[1, 2, 3]` ends with `[1, 2, 3]`; the facts of an item
//! that a `when` left out or that failed to render are not carried; a variable built from the fact
//! (`y: "{{ x | length }}"`) renders again with the new value; and the carried fact beats an
//! extra variable and a role parameter for the items of that loop only. The store keeps the old
//! rule: a failed task writes nothing, and the facts land once the task is judged.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::template::Templar;
use crate::vars::HostVars;

use super::Item;
use crate::executor::run::set_fact_names;

/// Whether the items of a controller-side loop render as the loop reaches them: measured on
/// ansible-core 2.19.12, a `set_fact` or `debug` loop with a `loop_control.pause` reads `now()`
/// after each pause, so its items cannot be rendered before the first one runs.
pub(super) fn paced(task: &crate::playbook::PlayTask, options: &super::LoopOptions) -> bool {
    options.pause.is_some_and(|pause| !pause.is_zero())
        && (super::is_local(&task.module) || crate::action_plugins::controller_side(&task.module))
}

/// Lays what item `ii - 1` set over item `ii` before it renders, when the items of a looped
/// `set_fact` render as the loop reaches them ([`paced`]): what [`Carry::vars`] and the item's
/// own pass do in `prepare` for an item rendered there.
pub(in crate::executor) fn carry_on(
    carry: Option<&mut Carry>,
    items: &mut [Item],
    ii: usize,
    templar: &Templar,
) {
    let (Some(carry), Some(index)) = (carry, ii.checked_sub(1)) else {
        return;
    };
    carry.take(&items[index], templar);
    let Some(item) = items.get_mut(ii).filter(|item| item.deferred) else {
        return;
    };
    for (name, template) in &carry.rebuilt {
        item.vars.insert(name.clone(), template.clone());
    }
    carry.lay_over(&mut item.vars);
    let (resolved, untrusted) = templar.resolve_vars_tainted(&item.vars);
    item.vars.map = resolved;
    item.vars.untrusted = untrusted;
}

/// What the items before the current one set, for one host.
pub(in crate::executor) struct Carry {
    /// The host's variables before they resolved each other: where the templates of `rebuilt`
    /// come from.
    merged: HostVars,
    facts: Map<String, Value>,
    /// The carried names whose value came from a managed host: they stay data.
    data: BTreeSet<String>,
    /// The carried names and the variables built from them, directly or through other
    /// variables: each name is followed once.
    reach: BTreeSet<String>,
    /// The templates, as the host wrote them, of the variables in `reach` that are not carried
    /// names. An item renders them again and takes every other variable as it resolved.
    rebuilt: Map<String, Value>,
}

impl Carry {
    pub(super) fn new(merged: HostVars) -> Self {
        Self {
            merged,
            facts: Map::new(),
            data: BTreeSet::new(),
            reach: BTreeSet::new(),
            rebuilt: Map::new(),
        }
    }

    /// The variables of the next item: `base`, with the carried facts laid over it. The
    /// variables built from a carried name go back to their templates, so that the pass that
    /// binds the loop variable renders them again. Only those are copied from the unresolved
    /// map, because the rest of it is as large as the host's variables.
    pub(super) fn vars(&self, base: &HostVars) -> HostVars {
        let mut vars = base.clone();
        if !self.facts.is_empty() {
            for (name, template) in &self.rebuilt {
                vars.insert(name.clone(), template.clone());
            }
            self.lay_over(&mut vars);
        }
        vars
    }

    /// Lays the carried facts over `vars`, each with the trust it was set with.
    pub(super) fn lay_over(&self, vars: &mut HostVars) {
        for (name, value) in &self.facts {
            if self.data.contains(name) {
                vars.insert_untrusted(name.clone(), value.clone());
            } else {
                vars.insert(name.clone(), value.clone());
            }
        }
    }

    /// Adds to `reach` every variable that reads a name already in it, until none is left.
    fn follow(&mut self) {
        loop {
            // A name that came from a managed host is data: no render touches it.
            let found: Vec<(String, Value)> = self
                .merged
                .map
                .iter()
                .filter(|(key, _)| !self.reach.contains(*key))
                .filter(|(key, _)| !self.merged.untrusted.contains(*key))
                .filter(|(_, value)| self.reach.iter().any(|name| mentions(value, name)))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            if found.is_empty() {
                return;
            }
            for (key, value) in found {
                self.reach.insert(key.clone());
                self.rebuilt.insert(key, value);
            }
        }
    }

    /// Takes what `item` sets: nothing for an item left out by its `when` or whose keys do not
    /// render. The trust is the one `record_local_facts` writes the same names with.
    pub(super) fn take(&mut self, item: &Item, templar: &Templar) {
        if item.skipped.is_some() {
            return;
        }
        let Ok(names) = set_fact_names(item, templar) else {
            return;
        };
        for (name, value, data) in names {
            if self.reach.insert(name.clone()) {
                self.follow();
            }
            if data {
                self.data.insert(name.clone());
            } else {
                self.data.remove(&name);
            }
            self.facts.insert(name, value.clone());
        }
    }
}

/// Whether a template inside `value` spells `name` as a word. A name inside a longer one does not
/// count; a quoted one does, for `vars['x']`.
fn mentions(value: &Value, name: &str) -> bool {
    match value {
        Value::String(text) => (text.contains("{{") || text.contains("{%")) && spells(text, name),
        Value::Array(items) => items.iter().any(|v| mentions(v, name)),
        Value::Object(map) => map.values().any(|v| mentions(v, name)),
        _ => false,
    }
}

fn spells(text: &str, name: &str) -> bool {
    let word = |c: char| c == '_' || c.is_ascii_alphanumeric();
    text.match_indices(name).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::{Carry, mentions};
    use crate::vars::HostVars;

    /// An item starts from the host's resolved variables, and only the variables built from a
    /// carried name go back to their templates: the others are not copied from the unresolved map.
    #[test]
    fn the_variables_an_item_renders_again_are_the_ones_built_from_a_carried_name() {
        let mut merged = HostVars::default();
        merged.map.insert("built".into(), json!("{{ x }}"));
        merged.map.insert("other".into(), json!("{{ 1 + 1 }}"));
        let mut base = HostVars::default();
        base.map.insert("built".into(), json!("old"));
        base.map.insert("other".into(), json!(2));
        let mut carry = Carry::new(merged);
        carry.facts.insert("x".into(), json!(1));
        carry.reach.insert("x".into());
        carry.follow();
        let vars = carry.vars(&base);
        assert_eq!(vars.get("built"), Some(&json!("{{ x }}")));
        assert_eq!(vars.get("other"), Some(&json!(2)));
        assert_eq!(vars.get("x"), Some(&json!(1)));
    }

    /// A variable built from a variable built from a carried name is followed as well.
    #[test]
    fn a_variable_two_hops_from_a_carried_name_is_followed() {
        let mut merged = HostVars::default();
        merged.map.insert("e".into(), json!("{{ [x] }}"));
        merged.map.insert("ee".into(), json!("{{ e | length }}"));
        merged.map.insert("other".into(), json!("{{ y }}"));
        let mut carry = Carry::new(merged);
        carry.reach.insert("x".into());
        carry.follow();
        assert!(carry.rebuilt.contains_key("ee"));
        assert!(carry.reach.contains("ee"));
        assert!(!carry.reach.contains("other"));
    }

    #[rstest]
    #[case::bare("{{ x | length }}", "x", true)]
    #[case::quoted("{{ vars['x'] }}", "x", true)]
    #[case::longer_name("{{ max }}", "x", false)]
    #[case::suffix("{{ x_list }}", "x", false)]
    #[case::attribute("{{ a.x }}", "x", true)]
    #[case::plain_text("x", "x", false)]
    #[case::nested(json!({"k": ["{{ x }}"]}), "x", true)]
    fn a_template_mentions_a_name_only_as_a_word(
        #[case] value: impl Into<serde_json::Value>,
        #[case] name: &str,
        #[case] found: bool,
    ) {
        assert_eq!(mentions(&value.into(), name), found);
    }
}
