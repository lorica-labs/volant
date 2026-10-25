// SPDX-License-Identifier: GPL-3.0-or-later
//! The `hostvars` object of a render.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use minijinja::value::{Enumerator, Object};
use serde_json::{Map, Value};

use super::super::templar::Templar;
use super::{Deferred, Memo, Tainted, Vars, context_tracking};
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
    pub(super) untrusted_hosts: BTreeSet<String>,
    /// Whether one of the host names came from a managed host: listing them taints the render.
    pub(super) keys_untrusted: bool,
    pub(super) tainted: Tainted,
    pub(super) deferred: Arc<Deferred>,
    pub(super) memo: Memo,
    pub(super) templar: Templar,
    /// The names of `listed` that hold host text, for the render of another host's variable.
    pub(super) listed_untrusted: BTreeSet<String>,
}

impl Object for Hostvars {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let key = key.as_str()?;
        // Before the memo, for the same reason as in `Context`.
        if !self.untrusted_hosts.contains(key) {
            return self.memo.get(key, || {
                self.hosts.get(key).map(|entry| {
                    crate::profile::count(crate::profile::Counter::RenderHostvars, 1);
                    let entry =
                        super::read::with_playbook_python(crate::vars::settled(entry).into_owned());
                    minijinja::Value::from_object(self.view_of(&entry))
                })
            });
        }
        self.tainted.store(true, Ordering::Relaxed);
        let found = self.memo.get(key, || {
            self.hosts.get(key).map(|entry| {
                crate::profile::count(crate::profile::Counter::RenderHostvars, 1);
                let entry =
                    super::read::with_playbook_python(crate::vars::settled(entry).into_owned());
                self.deferred.mark_host(key, &entry);
                crate::template::bigint::to_template(&entry)
            })
        });
        self.deferred.again_host(key);
        found
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
    /// One host's variables as a render of that host reads them.
    fn view_of(&self, entry: &Value) -> HostView {
        let empty = Map::new();
        let (ctx, tainted) = context_tracking(
            &self.templar,
            Vars {
                map: entry.as_object().unwrap_or(&empty),
                hostvars: None,
                shared: self.listed.as_ref(),
                facts: None,
                untrusted: Some(&self.listed_untrusted),
                untrusted_hosts: None,
                base: None,
            },
            true,
        );
        HostView {
            ctx,
            tainted,
            reader: Arc::clone(&self.tainted),
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
}

impl Object for HostView {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let found = self.ctx.as_object()?.get_value(key);
        if self.tainted.load(Ordering::Relaxed) {
            self.reader.store(true, Ordering::Relaxed);
        }
        found
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        self.ctx
            .as_object()
            .map_or(Enumerator::Empty, minijinja::value::DynObject::enumerate)
    }
}
