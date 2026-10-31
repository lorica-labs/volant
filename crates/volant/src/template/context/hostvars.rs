// SPDX-License-Identifier: GPL-3.0-or-later
//! The `hostvars` object of a render.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use minijinja::value::{Enumerator, Object};
use serde_json::{Map, Value};

use super::super::templar::Templar;
use super::{Memo, Tainted, Vars, context_tracking};
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
    /// Hosts holding at least one name that came from a managed host. Reading any name of such
    /// a host taints the render, which is coarser than the reference's per-string tag.
    pub(super) untrusted_hosts: crate::vars::UntrustedHosts,
    /// Whether one of the host names came from a managed host: listing them taints the render.
    pub(super) keys_untrusted: bool,
    pub(super) tainted: Tainted,
    pub(super) memo: Memo,
    pub(super) templar: Templar,
    /// The names of `listed` that hold host text, for the render of another host's variable.
    pub(super) listed_untrusted: BTreeSet<String>,
}

impl Object for Hostvars {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let key = key.as_str()?;
        // A host that holds host text, a registered result say, is read as any other, its
        // templates rendered, with the names that hold such text read as data: measured on
        // ansible-core 2.19.12, a group's `y: "{{ x }}"` reads `100` through `hostvars` after a
        // `register`. The read taints the render, which is coarser than the reference's
        // per-string tag.
        let data = self.untrusted_hosts.get(key);
        if data.is_some() {
            self.tainted.store(true, Ordering::Relaxed);
        }
        self.memo.get(key, || {
            self.hosts.get(key).map(|entry| {
                crate::profile::count(crate::profile::Counter::RenderHostvars, 1);
                let entry =
                    super::read::with_playbook_python(crate::vars::settled(entry).into_owned());
                let empty = BTreeSet::new();
                minijinja::Value::from_object(self.view_with(&entry, data.unwrap_or(&empty)))
            })
        })
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        if self.keys_untrusted {
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
    /// One host's variables as a render of that host reads them, the names of `data` read as data.
    fn view_with(&self, entry: &Value, data: &BTreeSet<String>) -> HostView {
        let untrusted: BTreeSet<String> = self.listed_untrusted.union(data).cloned().collect();
        let empty = Map::new();
        let (ctx, tainted) = context_tracking(
            &self.templar,
            Vars {
                map: entry.as_object().unwrap_or(&empty),
                hostvars: None,
                shared: self.listed.as_ref(),
                facts: None,
                untrusted: Some(&untrusted),
                untrusted_hosts: None,
                base: None,
            },
            true,
        );
        HostView {
            ctx,
            tainted,
            reader: Arc::clone(&self.tainted),
            written: entry.as_object().cloned().unwrap_or_default(),
            walked: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// `hostvars[name]` for a host that holds no host text: each variable read as a render of that
/// host would read it, a template in it rendered with that host's own variables and the
/// inventory's, as ansible-core 2.19.12 renders it. That render has no `hostvars`, as the
/// reference's has none. A variable that holds no template is handed out as it is, so text the
/// reader builds around it is never rendered a second time.
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
