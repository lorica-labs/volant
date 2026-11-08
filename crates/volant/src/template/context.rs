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
mod vars;

pub(in crate::template) use vars::copied;

use super::env::{chained_attr, fresh_const, mutable_attr, mutable_var};
use super::pymethods::mutable::{identity, mutable};
use super::templar::Templar;
use super::{
    ADD_KEY, ATTR_KEY, BYTES_ADD_KEY, CONCAT_KEY, CONST_KEY, CONTEXT_KEY, ITEM_KEY, MACRO_KEY,
    MATTR_KEY, MITEM_KEY, MKLIST_KEY, MKMAP_KEY, QUERY_KEY, REM_KEY, REMT_KEY, SLICE_KEY, SUB_KEY,
    Tainted, TemplateError, VAR_KEY,
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
    base: Arc<crate::tags::Tagged>,
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
pub(super) use read::{forgiving, take_failure};

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
    /// The tags of the values of `map`, by name.
    pub tags: Option<&'a Arc<crate::tags::TagMap>>,
    pub hostvars: Option<&'a Arc<crate::vars::HostvarsMap>>,
    /// The values the whole inventory shares — `groups` and the play's host lists — read out of
    /// here rather than copied into every host's map for every task. Consulted *before* `map`,
    /// because these used to be written into it last and last is what wins. A name written into
    /// the map later still wins, because `HostVars::insert` takes it out of here first.
    pub shared: Option<&'a Arc<Map<String, Value>>>,
    /// The names of `shared` that list a name `add_host` or `group_by` took from what a managed
    /// host sent. The strings of `shared` carry no tags, so the list as a whole answers for them.
    pub data_lists: Option<&'a Arc<BTreeSet<String>>>,
    /// The host's gathered facts, a layer shared rather than copied into `map` for every task.
    /// Consulted after `shared` and before `map`: whoever builds it leaves out every fact a layer
    /// of `map` outranks, so the facts left answer ahead of the layers below them.
    pub facts: Option<&'a Arc<Map<String, Value>>>,
    /// Variables an item shares with the others of its loop, read after `map`: a name in `map`
    /// answers first. A caller that owns its whole map leaves it empty.
    pub base: Option<&'a Arc<crate::tags::Tagged>>,
}

impl<'a> From<&'a Map<String, Value>> for Vars<'a> {
    fn from(map: &'a Map<String, Value>) -> Self {
        Vars {
            map,
            tags: None,
            hostvars: None,
            shared: None,
            data_lists: None,
            facts: None,
            base: None,
        }
    }
}

impl<'a> From<&'a crate::tags::Tagged> for Vars<'a> {
    fn from(tagged: &'a crate::tags::Tagged) -> Self {
        Vars {
            map: tagged,
            tags: Some(tagged.tags()),
            hostvars: None,
            shared: None,
            data_lists: None,
            facts: None,
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
    /// The tags of `vars` by name. `registry` holds those of the values the render read.
    pub(super) tags: Option<Arc<crate::tags::TagMap>>,
    pub(super) registry: super::tagged::TagRegistry,
    /// The value of the last expression, whose tags `registry` answers if something asks.
    pub(super) result: Mutex<Option<minijinja::Value>>,
    pub(super) base: Option<Arc<crate::tags::Tagged>>,
    pub(super) hostvars: Option<Arc<crate::vars::HostvarsMap>>,
    pub(super) shared: Option<Arc<Map<String, Value>>>,
    pub(super) data_lists: Option<Arc<BTreeSet<String>>>,
    pub(super) facts: Option<Arc<Map<String, Value>>>,
    tainted: Tainted,
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
    /// Whether `soiled` holds a name, so that a render that resolved nothing never takes its lock.
    any_soiled: AtomicBool,
    /// Set when `failed` is: the first error a nested render raised that the read cannot carry.
    failure: Mutex<Option<TemplateError>>,
    /// What each variable whose template read a name with no value failed with, and where that
    /// template was written.
    undefined_in: Mutex<HashMap<String, (String, Option<crate::playbook::Origin>)>>,
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

/// Starts a render over `ctx`: forgets what the last one changed in lists and mappings.
pub(super) fn begin(ctx: &minijinja::Value) {
    if let Some(root) = ctx.downcast_object_ref::<Context>() {
        root.forget_changes();
    }
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
    /// Marks the render as having read what came from outside the playbook: a managed host's
    /// text, or a file or a command read at run time.
    pub(in crate::template) fn note_outside_read(&self) {
        self.tainted.store(true, Ordering::Relaxed);
    }

    /// The host's own variables as one map: the base shared by the items of a loop, then the
    /// map over it.
    pub(super) fn flat_vars(&self) -> crate::tags::Tagged {
        let mut all = self.base.as_deref().cloned().unwrap_or_default();
        for (name, value) in &self.vars {
            let tree = self.tags.as_ref().and_then(|t| t.get(name)).cloned();
            all.insert(name.clone(), value.clone(), tree);
        }
        let (mut values, tags) = all.into_parts();
        complete_loop(&mut values);
        crate::tags::Tagged::new(values, tags)
    }
}

thread_local! {
    /// The last name a render looked for and did not find, which `mandatory` names in its
    /// refusal as the reference's undefined value carries its name.
    static MISSING: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The last name a render on this thread did not find, taken.
pub(in crate::template) fn take_missing() -> Option<String> {
    MISSING.with(|m| m.borrow_mut().take())
}

impl Object for Context {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let key = key.as_str()?;
        if key == "play_hosts" {
            PLAY_HOSTS_READ.set(true);
        }
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
            ADD_KEY,
            SUB_KEY,
            BYTES_ADD_KEY,
            SLICE_KEY,
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
        if let Some(name) = key.strip_prefix(MACRO_KEY) {
            return Some(super::macros::call(name.to_string()));
        }
        if let Some(name) = key.strip_prefix(VAR_KEY) {
            return Some(mutable_var(name.to_string()));
        }
        if key == CONTEXT_KEY {
            return Some(minijinja::Value::from_dyn_object(Arc::clone(self)));
        }
        // The magic `vars`, unless the host holds a variable of that name.
        if key == "vars" && !self.vars.contains_key(key) {
            return Some(minijinja::Value::from_object(vars::Vars(Arc::clone(self))));
        }
        // Before the memo, never after: a second read of the same name is answered from the
        // cache and would otherwise leave the render looking clean.
        if self.reads_host_text(key)
            || (self.any_soiled.load(Ordering::Relaxed)
                && self.soiled.lock().expect("template soiled").contains(key))
        {
            self.tainted.store(true, Ordering::Relaxed);
        }
        if let Some(value) = self.memo.lookup(key) {
            return Some(value);
        }
        let Some((value, keep)) = self.read(key) else {
            MISSING.with(|m| *m.borrow_mut() = Some(key.to_string()));
            return None;
        };
        let value = super::env::known(value);
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
    let tainted: Tainted = Arc::new(AtomicBool::new(false));
    let ctx = minijinja::Value::from_object(Context {
        vars: vars.map.clone(),
        tags: vars.tags.cloned(),
        registry: super::tagged::TagRegistry::default(),
        result: Mutex::new(None),
        base: vars.base.cloned(),
        hostvars: vars.hostvars.cloned(),
        shared: vars.shared.cloned(),
        data_lists: vars.data_lists.cloned(),
        facts: vars.facts.cloned(),
        tainted: Arc::clone(&tainted),
        memo: Memo::default(),
        mutables: Mutex::default(),
        mutated: AtomicBool::new(false),
        templar: templar.clone(),
        resolving: Mutex::new(Vec::new()),
        soiled: Mutex::new(BTreeSet::new()),
        any_soiled: AtomicBool::new(false),
        failure: Mutex::new(None),
        undefined_in: Mutex::default(),
        failed: AtomicBool::new(false),
    });
    (ctx, tainted)
}

thread_local! {
    /// Set when a render reads `play_hosts`, which the reference deprecates where it is read.
    static PLAY_HOSTS_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `render` and says whether it read `play_hosts`. A render runs on one thread from start
/// to end, so the flag it sets is its own; one watched inside another reports to both.
pub fn watch_play_hosts<T>(render: impl FnOnce() -> T) -> (T, bool) {
    let before = PLAY_HOSTS_READ.replace(false);
    let out = render();
    let read = PLAY_HOSTS_READ.get();
    PLAY_HOSTS_READ.set(before || read);
    (out, read)
}
