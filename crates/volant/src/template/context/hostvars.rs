// SPDX-License-Identifier: GPL-3.0-or-later
//! The `hostvars` object of a render.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use minijinja::value::{Enumerator, Object};
use serde_json::{Map, Value};

use super::super::HOSTVARS_KEYS;
use super::super::templar::Templar;
use super::{Memo, Tainted, Vars, context_of};
use crate::inventory::is_localhost;

/// `hostvars`: a map whose values are converted one host at a time. Iterating it still costs
/// the whole inventory, which is what a `hostvars | dict2items` asks for; reading one host
/// costs one host.
#[derive(Debug)]
pub(in crate::template) struct Hostvars {
    pub(in crate::template) hosts: Arc<crate::vars::HostvarsMap>,
    /// The inventory-wide values, whose `groups` say which hosts the inventory lists. The map
    /// answers for the three spellings of the controller whether they are listed or not, and an
    /// iteration shows only the listed ones, as the reference's does.
    pub(super) listed: Option<Arc<Map<String, Value>>>,
    /// Which of those list a name from a managed host, and whether the hosts of this map do.
    pub(super) data_lists: Option<Arc<BTreeSet<String>>>,
    pub(super) tainted: Tainted,
    pub(super) memo: Memo,
    pub(super) templar: Templar,
}

impl Object for Hostvars {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let key = key.as_str()?;
        self.memo.get(key, || {
            self.hosts.get(key).map(|entry| {
                crate::profile::count(crate::profile::Counter::RenderHostvars, 1);
                let view =
                    super::read::with_playbook_python(crate::vars::settled(entry).into_owned());
                minijinja::Value::from_object(self.view_with(&view, entry))
            })
        })
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        if self
            .data_lists
            .as_ref()
            .is_some_and(|d| d.contains(HOSTVARS_KEYS))
        {
            self.tainted.store(true, Ordering::Relaxed);
        }
        let unlisted: BTreeSet<&str> = match &self.listed {
            Some(shared) if self.hosts.keys().any(|k| is_localhost(k)) => {
                let all = shared
                    .get("groups")
                    .and_then(|groups| groups.get("all"))
                    .and_then(Value::as_array);
                let in_all = |name: &str| all.is_some_and(|a| a.iter().any(|h| h == name));
                self.hosts
                    .keys()
                    .map(String::as_str)
                    .filter(|k| is_localhost(k) && !in_all(k))
                    .collect()
            }
            _ => BTreeSet::new(),
        };
        Enumerator::Values(
            self.hosts
                .keys()
                .filter(|k| !unlisted.contains(k.as_str()))
                .map(|k| minijinja::Value::from(k.as_str()))
                .collect(),
        )
    }
}

impl Hostvars {
    /// One host's variables as a render of that host reads them.
    fn view_with(&self, view: &Value, entry: &crate::vars::HostEntry) -> HostView {
        let empty = Map::new();
        let (ctx, tainted) = context_of(
            &self.templar,
            Vars {
                map: view.as_object().unwrap_or(&empty),
                tags: Some(entry.tags()),
                hostvars: None,
                shared: self.listed.as_ref(),
                data_lists: self.data_lists.as_ref(),
                facts: None,
                base: None,
            },
        );
        HostView {
            ctx,
            tainted,
            reader: Arc::clone(&self.tainted),
            written: view.as_object().cloned().unwrap_or_default(),
            walked: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// `hostvars[name]`: each variable read as a render of that host would read it, a template in it
/// rendered with that host's own variables and the inventory's, as ansible-core 2.19.12 renders
/// it. That render has no `hostvars`, as the reference's has none. A variable that holds no
/// trusted template is handed out as it is, so text the reader builds around it is never rendered
/// a second time.
#[derive(Debug)]
struct HostView {
    ctx: minijinja::Value,
    /// Whether a render in `ctx` read host text, which the reader's render then holds too.
    tainted: Tainted,
    reader: Tainted,
    /// The host's variables as written, for one whose template does not render.
    written: Map<String, Value>,
    /// Whether the host is being read as a whole, every variable in turn.
    walked: std::sync::atomic::AtomicBool,
}

impl Object for HostView {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let found = self.ctx.as_object()?.get_value(key);
        if self.tainted.load(Ordering::Relaxed) {
            self.reader.store(true, Ordering::Relaxed);
        }
        // A variable whose template does not render, `rekey_on_member(defined_later)` read
        // before `defined_later` is, does not take the whole host down with it when the host is
        // read as a whole: it is its text there, and that text is data, never rendered again.
        // Read on its own it stays undefined, so `default` still answers for it.
        if self.walked.load(Ordering::Relaxed)
            && found.as_ref().is_none_or(minijinja::Value::is_undefined)
            && let Some(text) = key.as_str().and_then(|k| self.written.get(k))
        {
            self.reader.store(true, Ordering::Relaxed);
            return Some(crate::template::bigint::to_template(text));
        }
        found
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        self.walked.store(true, Ordering::Relaxed);
        self.ctx
            .as_object()
            .map_or(Enumerator::Empty, minijinja::value::DynObject::enumerate)
    }
}
