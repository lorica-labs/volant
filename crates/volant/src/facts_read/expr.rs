// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading one task and one expression for the facts it names.

use std::path::{Path, PathBuf};

use serde_json::Value;

use volant_protocol::modules::short_name;

use super::decide::{DYNAMIC_LOOKUPS, not_fact};
use super::lex::{close, is_word, leading_literal, leading_word, literal};
use super::scan::{Scan, Wanted};
use crate::compile::Origin;
use crate::executor::{expressions, task_strings};
use crate::playbook::PlayTask;

use crate::template::Templar;

impl Scan {
    /// Every file under `dir`, at any depth.
    pub(super) fn tree(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                self.tree(&path);
            } else {
                self.file(&path);
            }
        }
    }

    pub(super) fn text(&mut self, text: &str, place: &str) {
        for expr in expressions(text) {
            let first = expr
                .trim_start_matches(['-', '+'])
                .split_whitespace()
                .next();
            if let Some(pull @ ("include" | "import" | "from" | "extends")) = first {
                self.dynamic(&format!("{{% {pull} %}}"), place);
            }
            self.expression(expr, place);
        }
    }

    pub(super) fn task(&mut self, task: &PlayTask, origin: &Origin, place: &str) {
        let (text, bare) = task_strings(task);
        for s in text {
            self.text(s, place);
        }
        for s in bare {
            self.expression(s, place);
        }
        if let Some(lookup) = task.loop_with.as_deref()
            && DYNAMIC_LOOKUPS.contains(&short_name(lookup))
        {
            self.dynamic(&format!("with_{lookup}"), place);
        }
        for (name, value) in &task.vars {
            self.define(name, value);
        }
        if let Some(register) = &task.register {
            self.define(register, &Value::Null);
        }
        if task.loop_var != "item" {
            self.define(&task.loop_var, &Value::Null);
        }
        let literal = |key: &str| task.args.get(key).and_then(Value::as_str);
        match short_name(&task.module) {
            // The action plugins that read a fact themselves.
            "package" | "dnf" => self.key("pkg_mgr"),
            "service" => self.key("service_mgr"),
            "reboot" => {
                for key in ["distribution", "distribution_version", "os_family"] {
                    self.key(key);
                }
            }
            "set_fact" => {
                for (name, value) in &task.args {
                    match name.as_str() {
                        "cacheable" => {}
                        "_raw_params" => self.defs_unknown = true,
                        _ => self.define(name, value),
                    }
                }
            }
            "template" => match literal("src") {
                Some(src) if Templar::is_template(src) => {
                    self.later(Wanted::Template, src, task, origin, place);
                }
                Some(src) => {
                    if !self.find(origin, "templates", src) {
                        self.dynamic(&format!("template {src}, not found"), place);
                    }
                }
                None => self.dynamic("template with no literal src", place),
            },
            "include_vars" => {
                if let Some(name) = literal("name") {
                    self.define(name, &Value::Null);
                }
                match literal("file").or_else(|| literal("_raw_params")) {
                    Some(name) if Templar::is_template(name) => {
                        self.later(Wanted::Vars, name, task, origin, place);
                    }
                    Some(name) => {
                        if !self.find(origin, "vars", name) {
                            self.dynamic(&format!("include_vars {name}, not found"), place);
                        }
                    }
                    None => self.dynamic("include_vars", place),
                }
            }
            "include_tasks" => match literal("file").or_else(|| literal("_raw_params")) {
                Some(name) if Templar::is_template(name) => {
                    self.later(Wanted::Tasks, name, task, origin, place);
                }
                Some(_) => {}
                None => self.dynamic("include_tasks", place),
            },
            "include_role" => match literal("name").or_else(|| literal("role")) {
                Some(name) if !Templar::is_template(name) => {}
                _ => self.dynamic("include_role", place),
            },
            _ => {}
        }
    }

    /// One Jinja expression: what sits between the braces, or a bare condition.
    pub(super) fn expression(&mut self, e: &str, place: &str) {
        let b = e.as_bytes();
        let mut i = 0;
        // The quote character of the string literal the scan is in, and where that literal opens.
        let mut quote: Option<(u8, usize)> = None;
        while i < b.len() {
            let c = b[i];
            match quote {
                Some(_) if c == b'\\' => {
                    i += 2;
                    continue;
                }
                Some((q, _)) if c == q => {
                    quote = None;
                    i += 1;
                    continue;
                }
                None if c == b'\'' || c == b'"' => {
                    if let Some(lit) = leading_literal(&e[i..]) {
                        let lookup = e[..i]
                            .trim_end()
                            .strip_suffix('(')
                            .map(str::trim_end)
                            .is_some_and(|f| {
                                ["lookup", "query", "q"].iter().any(|name| {
                                    f.strip_suffix(name).is_some_and(|b| {
                                        !b.bytes().last().is_some_and(|c| is_word(c) || c == b'.')
                                    })
                                })
                            });
                        if lookup && DYNAMIC_LOOKUPS.contains(&short_name(lit)) {
                            self.dynamic(&format!("lookup('{lit}')"), place);
                        }
                    }
                    quote = Some((c, i));
                    i += 1;
                    continue;
                }
                _ => {}
            }
            if (c.is_ascii_alphabetic() || c == b'_') && (i == 0 || !is_word(b[i - 1])) {
                let start = i;
                while i < b.len() && is_word(b[i]) {
                    i += 1;
                }
                self.word(e, start, i, quote, place);
                continue;
            }
            i += 1;
        }
    }

    pub(super) fn word(
        &mut self,
        e: &str,
        start: usize,
        end: usize,
        quote: Option<(u8, usize)>,
        place: &str,
    ) {
        let word = &e[start..end];
        if word == "ansible_facts" {
            return self.facts(e, start, end, quote, place);
        }
        // Puppet's and Chef's facts, which the Python `setup` adds without a prefix when their
        // tool is on the host, and the native never collects.
        if word.starts_with("facter_") || word.starts_with("ohai_") {
            return self.key(word);
        }
        if let Some(key) = word.strip_prefix("ansible_") {
            if key.is_empty() {
                self.dynamic("'ansible_' joined to a name", place);
            } else if !not_fact(key) {
                self.key(key);
            }
            return;
        }
        if quote.is_some() || (start > 0 && e.as_bytes()[start - 1] == b'.') {
            return;
        }
        match word {
            "vars" => self.dynamic("vars", place),
            "hostvars" => self.hostvars(e, start, end, place),
            _ => {}
        }
    }

    /// `ansible_facts` at `start..end`.
    pub(super) fn facts(
        &mut self,
        e: &str,
        start: usize,
        end: usize,
        quote: Option<(u8, usize)>,
        place: &str,
    ) {
        let rest = &e[end..];
        let Some((q, open)) = quote else {
            return self.facts_access(rest, place);
        };
        // A dotted path in a string, as `selectattr` and `map(attribute=)` take one.
        if rest.starts_with('.') {
            return self.facts_access(rest, place);
        }
        // `hostvars[h]['ansible_facts'][...]`, or `extract`'s list `['ansible_facts', 'x']`.
        let whole = open + 1 == start && rest.as_bytes().first() == Some(&q);
        if whole && e[..open].trim_end().ends_with('[') {
            let after = rest[1..].trim_start();
            if let Some(next) = after.strip_prefix(']') {
                return self.facts_access(next, place);
            }
            if let Some(key) = after
                .strip_prefix(',')
                .and_then(|next| leading_literal(next.trim_start()))
            {
                return self.key(key);
            }
        }
        self.dynamic("'ansible_facts' as a whole", place);
    }

    /// What follows `ansible_facts`: `.name` or `['name']` read one key, anything else all.
    pub(super) fn facts_access(&mut self, rest: &str, place: &str) {
        if let Some(after) = rest.strip_prefix('.') {
            let name = leading_word(after);
            if !name.is_empty() && !after[name.len()..].starts_with('(') {
                return self.key(name);
            }
            return self.dynamic(&format!("ansible_facts.{name}("), place);
        }
        if rest.starts_with('[') {
            match close(rest, 0) {
                Some(end) => match literal(&rest[1..end]) {
                    Some(key) => self.key(key),
                    None => self.dynamic(&format!("ansible_facts{}", &rest[..=end]), place),
                },
                None => self.dynamic("ansible_facts[", place),
            }
            return;
        }
        self.dynamic("ansible_facts", place);
    }

    /// `hostvars` at `start..end`, outside a string. One variable of one host is read by name
    /// (the fact names in it are read by the string and word rules); a host's whole set, or
    /// `hostvars` itself, reads every fact.
    pub(super) fn hostvars(&mut self, e: &str, start: usize, end: usize, place: &str) {
        let rest = &e[end..];
        if rest.starts_with('[') {
            let Some(host_end) = close(rest, 0) else {
                return self.dynamic("hostvars[", place);
            };
            let after = &rest[host_end + 1..];
            if let Some(a) = after.strip_prefix('.') {
                let name = leading_word(a);
                if name.is_empty() || a[name.len()..].starts_with('(') {
                    self.dynamic(&format!("hostvars[...].{name}("), place);
                }
                return;
            }
            if after.starts_with('[') {
                if !close(after, 0).is_some_and(|c| literal(&after[1..c]).is_some()) {
                    self.dynamic("hostvars[...][<expression>]", place);
                }
                return;
            }
            return self.dynamic(&format!("hostvars{} used whole", &rest[..=host_end]), place);
        }
        // `map('extract', hostvars, <key>)`, and `map('extract', hostvars)` whose hosts are only
        // looked into by `selectattr`/`rejectattr` and then `map(attribute=)`.
        let before = e[..start].trim_end();
        let extract = before
            .strip_suffix(',')
            .map(str::trim_end)
            .is_some_and(|b| b.ends_with("'extract'") || b.ends_with("\"extract\""));
        if extract {
            let after = rest.trim_start();
            if let Some(key) = after.strip_prefix(',').map(str::trim_start) {
                let listed = key.starts_with('[')
                    && close(key, 0)
                        .is_some_and(|c| key[1..c].split(',').all(|k| literal(k).is_some()));
                if leading_literal(key).is_some() || listed {
                    return;
                }
                return self.dynamic("map('extract', hostvars, <expression>)", place);
            }
            if after.strip_prefix(')').is_some_and(attributes_only) {
                return;
            }
        }
        self.dynamic("hostvars used whole", place);
    }
}

/// Whether what follows `map('extract', hostvars)` only looks into each host by a literal
/// attribute: any number of `selectattr('...', ...)`/`rejectattr('...', ...)`, then
/// `map(attribute='...')`. An attribute named by a variable could name any fact.
pub(super) fn attributes_only(chain: &str) -> bool {
    let mut s = chain.trim_start();
    loop {
        let Some(next) = s.strip_prefix('|').map(str::trim_start) else {
            return false;
        };
        if next.starts_with("selectattr(") || next.starts_with("rejectattr(") {
            let open = next.find('(').unwrap_or_default();
            let Some(end) = close(next, open) else {
                return false;
            };
            if !literal_first(&next[open + 1..end]) {
                return false;
            }
            s = next[end + 1..].trim_start();
            continue;
        }
        return next
            .strip_prefix("map(")
            .and_then(|m| m.trim_start().strip_prefix("attribute="))
            .is_some_and(literal_first);
    }
}

/// Whether `args` opens with a string literal that ends the argument.
pub(super) fn literal_first(args: &str) -> bool {
    let args = args.trim_start();
    leading_literal(args).is_some_and(|lit| {
        let after = args[lit.len() + 2..].trim_start();
        after.is_empty() || after.starts_with(',') || after.starts_with(')')
    })
}

/// The variable `s` is when it is `{{ name }}` and nothing else.
pub(super) fn bare_variable(s: &str) -> Option<&str> {
    let name = s.trim().strip_prefix("{{")?.strip_suffix("}}")?.trim();
    (!name.is_empty() && name.bytes().all(is_word)).then_some(name)
}
