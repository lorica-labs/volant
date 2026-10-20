// SPDX-License-Identifier: GPL-3.0-or-later
//! What a render reads a name from: the variables, which of them came from a host, and
//! the sink that records a read of one.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use minijinja::value::{Enumerator, Object};
use serde_json::{Map, Value};

mod hostvars;
mod read;

use super::env::{chained_attr, fresh_const, mutable_attr, mutable_var};
use super::pymethods::mutable::{identity, mutable};
use super::templar::Templar;
use super::{
    ATTR_KEY, CONCAT_KEY, CONST_KEY, CONTEXT_KEY, HOSTVARS_KEYS, ITEM_KEY, MATTR_KEY, MITEM_KEY,
    MKLIST_KEY, MKMAP_KEY, QUERY_KEY, REM_KEY, REMT_KEY, TAINT_KEY, Tainted, TemplateError,
    VAR_KEY,
};

/// The name, in the variables an extended loop shares between its items, under which the list of
/// all the items is kept once. It is no variable: a render never finds it by name, it reads it
/// through `ansible_loop.allitems`, and `HostVars::own_variables` puts it back into `ansible_loop`.
pub const ALLITEMS_KEY: &str = "volant::allitems";

/// The variable an extended loop binds.
const LOOP_KEY: &str = "ansible_loop";

/// The fields of `ansible_loop` that sit before `allitems`, as the reference lists them.
const BEFORE_ALLITEMS: &str = "revindex0";

/// `fields` with the list of all the items in its place.
pub fn with_allitems(fields: &Map<String, Value>, all: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in fields {
        out.insert(key.clone(), value.clone());
        if key == BEFORE_ALLITEMS {
            out.insert("allitems".into(), all.clone());
        }
    }
    out.entry("allitems").or_insert_with(|| all.clone());
    out
}

/// `ansible_loop` of an extended loop: its own fields, and `allitems` read from the one list the
/// whole loop shares, converted only when a template asks for it.
#[derive(Debug)]
struct LoopView {
    fields: Map<String, Value>,
    base: Arc<Map<String, Value>>,
}

impl Object for LoopView {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let key = key.as_str()?;
        let found = if key == "allitems" {
            self.base.get(ALLITEMS_KEY)
        } else {
            self.fields.get(key)
        };
        found.map(super::bigint::to_template)
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let mut keys = Vec::new();
        for key in self.fields.keys() {
            keys.push(minijinja::Value::from(key.as_str()));
            if key == BEFORE_ALLITEMS {
                keys.push(minijinja::Value::from("allitems"));
            }
        }
        Enumerator::Values(keys)
    }
}

pub(super) use hostvars::Hostvars;
use read::Deferred;
pub(super) use read::take_failure;

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
    /// Variables an item shares with the others of its loop, read after `map`: a name in `map`
    /// answers first. A caller that owns its whole map leaves it empty.
    pub base: Option<&'a Arc<Map<String, Value>>>,
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
            base: None,
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
    pub(super) fn lookup(&self, key: &str) -> Option<minijinja::Value> {
        self.0.lock().expect("template memo").get(key).cloned()
    }

    /// Not held across the conversion: resolving a variable renders a template that reads other
    /// variables, and those reads come back here.
    pub(super) fn store(&self, key: &str, value: &minijinja::Value) {
        self.0
            .lock()
            .expect("template memo")
            .insert(key.to_string(), value.clone());
    }

    pub(super) fn get(
        &self,
        key: &str,
        convert: impl FnOnce() -> Option<minijinja::Value>,
    ) -> Option<minijinja::Value> {
        if let Some(value) = self.lookup(key) {
            return Some(value);
        }
        let value = convert()?;
        self.store(key, &value);
        Some(value)
    }
}

/// The root of a render: the host's variables, each converted when it is asked for rather than
/// all of them up front, and `hostvars` as an object that hands out one host's view on demand.
#[derive(Debug)]
pub(super) struct Context {
    pub(super) vars: Map<String, Value>,
    pub(super) base: Option<Arc<Map<String, Value>>>,
    pub(super) hostvars: Option<Arc<Map<String, Value>>>,
    pub(super) shared: Option<Arc<Map<String, Value>>>,
    pub(super) facts: Option<Arc<Map<String, Value>>>,
    pub(super) untrusted: BTreeSet<String>,
    /// Whether the caller tracks which names hold host text: `untrusted` is then complete. Without
    /// it, a read of the inventory's names cannot be told from a read of host text.
    pub(super) tracked: bool,
    pub(super) untrusted_hosts: BTreeSet<String>,
    tainted: Tainted,
    /// Set when a pass left text that must stay as it is: see `Templar::render_in`. Not data:
    /// nothing is refused because of it.
    literal: AtomicBool,
    /// The reads that handed out a template unrendered, to tell a pass whose markers are its own.
    deferred: Arc<Deferred>,
    memo: Memo,
    /// The mutable form of each plain list or mapping read while one string renders, by the
    /// address of the plain one, which it keeps alive. Emptied before the next string.
    mutables: Mutex<HashMap<usize, (minijinja::Value, minijinja::Value)>>,
    /// Whether `mutables` holds anything, so that a render that changes no list never locks it.
    mutated: AtomicBool,
    templar: Templar,
    /// The names whose value is being rendered now, in the order the reads nested.
    resolving: Mutex<Vec<String>>,
    /// Names whose rendered value was built from something a managed host wrote. A read of one is
    /// as much a read of host data as a read of the name it was built from.
    soiled: Mutex<BTreeSet<String>>,
    /// Whether the text of a variable reads only what the playbook wrote, name by name.
    author_only: Mutex<HashMap<String, bool>>,
    /// Whether `soiled` holds a name, so that a render that resolved nothing never takes its lock.
    any_soiled: AtomicBool,
    /// Set when `failed` is: the first error a nested render raised that the read cannot carry.
    failure: Mutex<Option<TemplateError>>,
    failed: AtomicBool,
}

impl Context {
    /// The mutable form of `value` when it is a plain list or mapping: the same object for each
    /// read of the same value, so that two names for one list name one list.
    pub(super) fn mutable_read(&self, value: minijinja::Value) -> minijinja::Value {
        let Some(id) = identity(&value) else {
            return value;
        };
        self.mutated.store(true, Ordering::Relaxed);
        let mut seen = self.mutables.lock().unwrap_or_else(PoisonError::into_inner);
        seen.entry(id)
            .or_insert_with(|| {
                (
                    value.clone(),
                    mutable(&value).unwrap_or_else(|| value.clone()),
                )
            })
            .1
            .clone()
    }

    /// Forgets what one string changed, so that the next one reads the variables as they were.
    pub(super) fn forget_changes(&self) {
        if !self.mutated.swap(false, Ordering::Relaxed) {
            return;
        }
        self.mutables
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// Starts a render over `ctx`: clears the mark of the last one and what it changed in lists and
/// mappings, and returns the deferred reads so far.
pub(super) fn begin(ctx: &minijinja::Value) -> usize {
    set_literal(ctx, false);
    if let Some(root) = ctx.downcast_object_ref::<Context>() {
        root.forget_changes();
    }
    deferred(ctx)
}

/// Records that the render over `ctx` left text that no further pass may render.
pub(super) fn set_literal(ctx: &minijinja::Value, on: bool) {
    if let Some(root) = ctx.downcast_object_ref::<Context>() {
        root.literal.store(on, Ordering::Relaxed);
    }
}

/// How many reads of the renders over `ctx` handed out a template unrendered so far.
pub(super) fn deferred(ctx: &minijinja::Value) -> usize {
    ctx.downcast_object_ref::<Context>()
        .map_or(0, |root| root.deferred.count())
}

/// Whether the render over `ctx` left such text.
pub(super) fn is_literal(ctx: &minijinja::Value) -> bool {
    ctx.downcast_object_ref::<Context>()
        .is_some_and(|root| root.literal.load(Ordering::Relaxed))
}

/// Puts the shared list of all the items back into `ansible_loop` and drops the shared entry.
pub fn complete_loop(all: &mut Map<String, Value>) {
    let Some(list) = all.shift_remove(ALLITEMS_KEY) else {
        return;
    };
    if let Some(Value::Object(fields)) = all.get(LOOP_KEY) {
        let whole = with_allitems(fields, &list);
        all.insert(LOOP_KEY.into(), Value::Object(whole));
    }
}

impl Context {
    /// The host's own variables as one map: the base shared by the items of a loop, then the
    /// map over it.
    pub(super) fn flat_vars(&self) -> Map<String, Value> {
        let mut all = self.base.as_deref().cloned().unwrap_or_default();
        all.extend(self.vars.iter().map(|(k, v)| (k.clone(), v.clone())));
        complete_loop(&mut all);
        all
    }
}

impl Object for Context {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let key = key.as_str()?;
        // Not a variable: the VM looks the root up before the globals, so a variable of that
        // name (`-e '{"volant::concat": 1}'`) would otherwise replace every `~`.
        if [
            CONCAT_KEY,
            QUERY_KEY,
            ITEM_KEY,
            MITEM_KEY,
            MKLIST_KEY,
            MKMAP_KEY,
            REM_KEY,
            REMT_KEY,
            ALLITEMS_KEY,
        ]
        .contains(&key)
            || super::order::is_key(key)
        {
            return None;
        }
        if let Some(attr) = key.strip_prefix(ATTR_KEY) {
            return Some(chained_attr(attr.to_string()));
        }
        if let Some(attr) = key.strip_prefix(MATTR_KEY) {
            return Some(mutable_attr(attr.to_string()));
        }
        if let Some(json) = key.strip_prefix(CONST_KEY) {
            return fresh_const(json);
        }
        if let Some(name) = key.strip_prefix(VAR_KEY) {
            return Some(mutable_var(name.to_string()));
        }
        if key == TAINT_KEY {
            return Some(minijinja::Value::from_object(TaintSink(Arc::clone(
                &self.tainted,
            ))));
        }
        if key == CONTEXT_KEY {
            return Some(minijinja::Value::from_dyn_object(Arc::clone(self)));
        }
        self.deferred.again(key);
        // Before the memo, never after: a second read of the same name is answered from the
        // cache and would otherwise leave the render looking clean.
        // A `hostvars` the map carries as a plain value lists its hosts on any read, so a host
        // name that came from a managed host taints every read of it.
        let untrusted = self.untrusted.contains(key);
        if untrusted
            || (self.any_soiled.load(Ordering::Relaxed)
                && self.soiled.lock().expect("template soiled").contains(key))
            || (key == "hostvars"
                && self.hostvars.is_none()
                && self.untrusted.contains(HOSTVARS_KEYS))
        {
            self.tainted.store(true, Ordering::Relaxed);
        }
        if let Some(value) = self.memo.lookup(key) {
            return Some(value);
        }
        let (value, keep) = self.read(key, untrusted)?;
        if keep {
            self.memo.store(key, &value);
        }
        Some(value)
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        // The host's own names, `map` over the base a loop's items share: a later layer lists
        // only the names neither of them holds, so a name is never listed twice.
        let own = |key: &str| {
            self.vars.contains_key(key) || self.base.as_ref().is_some_and(|b| b.contains_key(key))
        };
        let mut keys: Vec<minijinja::Value> = self
            .vars
            .keys()
            .map(|k| minijinja::Value::from(k.as_str()))
            .collect();
        for key in self.base.iter().flat_map(|b| b.keys()) {
            if !self.vars.contains_key(key) && key != ALLITEMS_KEY {
                keys.push(minijinja::Value::from(key.as_str()));
            }
        }
        if self.hostvars.is_some() && !own("hostvars") {
            keys.push(minijinja::Value::from("hostvars"));
        }
        for key in self.shared.iter().flat_map(|s| s.keys()) {
            if !own(key) {
                keys.push(minijinja::Value::from(key.as_str()));
            }
        }
        for key in self.facts.iter().flat_map(|f| f.keys()) {
            if !own(key) && !self.shared.as_ref().is_some_and(|s| s.contains_key(key)) {
                keys.push(minijinja::Value::from(key.as_str()));
            }
        }
        Enumerator::Values(keys)
    }
}

pub(super) fn context_of(templar: &Templar, vars: Vars<'_>) -> (minijinja::Value, Tainted) {
    context_tracking(templar, vars, vars.untrusted.is_some())
}

/// `context_of` for a caller that supplies `untrusted` itself and says whether it is complete.
pub(super) fn context_tracking(
    templar: &Templar,
    vars: Vars<'_>,
    tracked: bool,
) -> (minijinja::Value, Tainted) {
    let tainted: Tainted = Arc::new(AtomicBool::new(false));
    let ctx = minijinja::Value::from_object(Context {
        vars: vars.map.clone(),
        base: vars.base.cloned(),
        hostvars: vars.hostvars.cloned(),
        shared: vars.shared.cloned(),
        facts: vars.facts.cloned(),
        untrusted: vars.untrusted.cloned().unwrap_or_default(),
        tracked,
        untrusted_hosts: vars.untrusted_hosts.cloned().unwrap_or_default(),
        tainted: Arc::clone(&tainted),
        literal: AtomicBool::new(false),
        deferred: Arc::new(Deferred::default()),
        memo: Memo::default(),
        mutables: Mutex::default(),
        mutated: AtomicBool::new(false),
        templar: templar.clone(),
        resolving: Mutex::new(Vec::new()),
        soiled: Mutex::new(BTreeSet::new()),
        author_only: Mutex::new(HashMap::new()),
        any_soiled: AtomicBool::new(false),
        failure: Mutex::new(None),
        failed: AtomicBool::new(false),
    });
    (ctx, tainted)
}
