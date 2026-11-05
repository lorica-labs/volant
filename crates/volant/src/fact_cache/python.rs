// SPDX-License-Identifier: GPL-3.0-or-later
//! A fact cache plugin of ansible-core's, called in the run's Python helper.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::{CacheFailure, untag};
use crate::bridge::Bridge;

/// One plugin, by the name a setting gives it.
#[derive(Clone)]
pub struct PythonCache {
    plugin: String,
    bridge: Arc<Bridge>,
}

impl std::fmt::Debug for PythonCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PythonCache({})", self.plugin)
    }
}

impl PythonCache {
    pub(super) fn new(plugin: &str, bridge: Arc<Bridge>) -> Self {
        Self {
            plugin: plugin.to_string(),
            bridge,
        }
    }

    pub(super) fn load(&self) -> Result<(), String> {
        let answer = self
            .bridge
            .cache(&self.plugin, &json!(["load"]))
            .map_err(|err| err.to_string())?;
        match answer.get("warning").and_then(Value::as_str) {
            Some(why) => Err(why.to_string()),
            None => Ok(()),
        }
    }

    pub(super) fn read(&self, host: &str) -> Result<Option<Map<String, Value>>, CacheFailure> {
        let answer = self
            .bridge
            .cache(&self.plugin, &json!(["get", host]))
            .map_err(|err| CacheFailure {
                warning: None,
                error: format!("Unexpected Exception, this is probably a bug: {err}"),
            })?;
        Ok(answer.get("facts").and_then(Value::as_object).cloned())
    }

    /// The plugin gets the facts plain: the tags of [`super::tag`] are the `jsonfile` file's.
    pub(super) fn write(&self, host: &str, facts: &Map<String, Value>) -> std::io::Result<()> {
        let plain = untag(Value::Object(facts.clone()));
        self.bridge
            .cache(&self.plugin, &json!(["set", host, plain]))
            .map(drop)
            .map_err(std::io::Error::other)
    }

    pub(super) fn flush<'a>(
        &self,
        hosts: impl IntoIterator<Item = &'a str>,
    ) -> std::io::Result<()> {
        for host in hosts {
            self.bridge
                .cache(&self.plugin, &json!(["delete", host]))
                .map_err(std::io::Error::other)?;
        }
        Ok(())
    }
}
