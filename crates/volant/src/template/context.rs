// SPDX-License-Identifier: GPL-3.0-or-later
//! What a render reads a name from: the variables, which of them came from a host, and
//! the sink that records a read of one.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use minijinja::value::{Enumerator, Object};
use serde_json::{Map, Value};

use super::env::chained_attr;
use super::{ATTR_KEY, CONCAT_KEY, CONTEXT_KEY, ITEM_KEY, QUERY_KEY, TAINT_KEY, Tainted};

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
    pub(super) fn get(
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
pub(super) struct Context {
    pub(super) vars: Map<String, Value>,
    pub(super) hostvars: Option<Arc<Map<String, Value>>>,
    pub(super) shared: Option<Arc<Map<String, Value>>>,
    pub(super) facts: Option<Arc<Map<String, Value>>>,
    pub(super) untrusted: BTreeSet<String>,
    pub(super) untrusted_hosts: BTreeSet<String>,
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
pub(super) struct Hostvars {
    pub(super) hosts: Arc<Map<String, Value>>,
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

pub(super) fn context_of(vars: Vars<'_>) -> (minijinja::Value, Tainted) {
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
