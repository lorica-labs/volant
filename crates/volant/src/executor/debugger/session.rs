// SPDX-License-Identifier: GPL-3.0-or-later
//! One debugger prompt: the commands of ansible-core 2.19.12's `Debugger(cmd.Cmd)` and the
//! names its expressions read, `task`, `task_vars`, `host` and `result`.

use std::sync::Arc;

use serde_json::{Map, Value};

use super::expr::{self, Expr, PyError, Statement};
use crate::render::Renderer;
use crate::template::Templar;
use crate::vars::HostVars;

const HELP: &str = "List available commands with \"help\" or detailed help with \"help cmd\".";
const REDO: &str = "Schedule task for re-execution. The re-execution may not be the next result";
const UPDATE: &str = "Recreate the task from ``task._ds``, and template with updated ``task_vars``";

/// Every command and its help text, in the order `help` lists them.
const COMMANDS: [(&str, &str); 13] = [
    ("EOF", "Quit"),
    ("c", "Continue to next result"),
    ("continue", "Continue to next result"),
    ("h", HELP),
    ("help", HELP),
    ("p", "Pretty Print"),
    ("pprint", "Pretty Print"),
    ("q", "Quit"),
    ("quit", "Quit"),
    ("r", REDO),
    ("redo", REDO),
    ("u", UPDATE),
    ("update_task", UPDATE),
];

/// What the prompt does after a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::executor) enum Next {
    Stay,
    Continue,
    Redo,
    Quit,
}

/// The state one prompt reads and edits.
pub(in crate::executor) struct Session {
    pub(in crate::executor) host: String,
    /// The task as its banner names it, which is what `task` prints after `TASK: `.
    pub(in crate::executor) title: String,
    /// `task.name`, as written.
    pub(in crate::executor) name: String,
    /// `task.action`.
    pub(in crate::executor) action: String,
    /// The task's arguments as written, which `u` renders again.
    pub(in crate::executor) raw_args: Map<String, Value>,
    /// `task.args`: the arguments rendered with `task_vars`.
    pub(in crate::executor) args: Map<String, Value>,
    /// The arguments the task ran with: `r` hands `args` over when they differ from these.
    pub(in crate::executor) ran: Map<String, Value>,
    /// `task_vars`.
    pub(in crate::executor) vars: HostVars,
    /// What `result._return_data` holds.
    pub(in crate::executor) result: Map<String, Value>,
    pub(in crate::executor) templar: Arc<Templar>,
    /// Names bound at the prompt, `x = 1`.
    pub(in crate::executor) locals: Map<String, Value>,
    /// The last command, which an empty line repeats.
    pub(in crate::executor) last: String,
}

/// A value an expression produces.
#[derive(Debug, Clone, PartialEq)]
enum Py {
    Json(Value),
    Task,
    Host,
    Result,
    /// `dict_keys`, `dict_values`: the view's name and its elements.
    View(&'static str, Vec<Value>),
    Items(Vec<(String, Value)>),
}

impl Session {
    /// The arguments rendered with the current `task_vars`, which is what `u` does to the task.
    pub(in crate::executor) fn render_args(&self) -> Result<Map<String, Value>, String> {
        match self
            .templar
            .render_value(&Value::Object(self.raw_args.clone()), &self.vars)
        {
            Ok(Value::Object(map)) => Ok(map),
            Ok(other) => Err(format!("the arguments rendered to {other}")),
            Err(err) => Err(err.0),
        }
    }

    /// Runs one line the way `cmd.Cmd.onecmd` dispatches it.
    pub(in crate::executor) fn onecmd(&mut self, line: &str, out: &mut Renderer) -> Next {
        let mut line = line.trim().to_string();
        if line.is_empty() {
            if self.last.is_empty() {
                return Next::Stay;
            }
            line.clone_from(&self.last);
        }
        if let Some(rest) = line.strip_prefix('?') {
            line = format!("help {rest}");
        }
        if line.starts_with('!') {
            self.default(&line, out);
            return Next::Stay;
        }
        let split = line
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(line.len());
        let (cmd, arg) = (&line[..split], line[split..].trim());
        self.last = if line == "EOF" {
            String::new()
        } else {
            line.clone()
        };
        match cmd {
            "c" | "continue" => return Next::Continue,
            "q" | "quit" | "EOF" => return Next::Quit,
            "h" | "help" => help(arg, out),
            "p" | "pprint" => match expr::expression(arg).and_then(|e| self.eval(&e)) {
                Ok(value) => out.display(&self.pformat(&value)),
                Err(err) => out.display(&err.line(arg, true)),
            },
            "r" | "redo" => return Next::Redo,
            "u" | "update_task" => match self.render_args() {
                Ok(args) => self.args = args,
                Err(msg) => out.display(&format!("***{msg}")),
            },
            _ => {
                let line = line.clone();
                self.default(&line, out);
            }
        }
        Next::Stay
    }

    /// A line that is no command, run as a Python statement: an expression prints its `repr`
    /// unless it is `None`, an assignment prints nothing.
    fn default(&mut self, line: &str, out: &mut Renderer) {
        let done = expr::statement(line).and_then(|s| match s {
            Statement::Expr(e) => self.eval(&e).map(|v| {
                if v != Py::Json(Value::Null) {
                    out.display(&self.repr(&v));
                }
            }),
            Statement::Assign(target, value) => {
                let value = self.eval(&value).and_then(|v| self.json(v))?;
                self.assign(&target, value)
            }
        });
        if let Err(err) = done {
            out.display(&err.line(line, false));
        }
    }

    fn task_vars(&self) -> Map<String, Value> {
        let mut all = self.vars.map.clone();
        for (k, v) in self.vars.facts.iter().chain(self.vars.shared.iter()) {
            all.entry(k.clone()).or_insert_with(|| v.clone());
        }
        all.entry("hostvars".to_string())
            .or_insert_with(|| Value::Object((*self.vars.hostvars).clone()));
        all
    }

    fn json(&self, value: Py) -> Result<Value, PyError> {
        match value {
            Py::Json(v) => Ok(v),
            Py::View(_, items) => Ok(Value::Array(items)),
            Py::Items(items) => Ok(Value::Array(
                items
                    .into_iter()
                    .map(|(k, v)| Value::Array(vec![Value::String(k), v]))
                    .collect(),
            )),
            Py::Task | Py::Host | Py::Result => Ok(Value::String(self.repr(&value))),
        }
    }

    fn eval(&self, e: &Expr) -> Result<Py, PyError> {
        match e {
            Expr::Name(name) => match name.as_str() {
                _ if self.locals.contains_key(name) => Ok(Py::Json(self.locals[name].clone())),
                "task" => Ok(Py::Task),
                "host" => Ok(Py::Host),
                "result" => Ok(Py::Result),
                "task_vars" => Ok(Py::Json(Value::Object(self.task_vars()))),
                _ => Err(PyError::Name(name.clone())),
            },
            Expr::Literal(v) => Ok(Py::Json(v.clone())),
            Expr::List(items) => Ok(Py::Json(Value::Array(
                items
                    .iter()
                    .map(|i| self.eval(i).and_then(|v| self.json(v)))
                    .collect::<Result<_, _>>()?,
            ))),
            Expr::Dict(pairs) => {
                let mut map = Map::new();
                for (k, v) in pairs {
                    let key = match self.eval(k).and_then(|k| self.json(k))? {
                        Value::String(s) => s,
                        other => expr::repr(&other, false),
                    };
                    map.insert(key, self.eval(v).and_then(|v| self.json(v))?);
                }
                Ok(Py::Json(Value::Object(map)))
            }
            Expr::Attr(base, name) => self.attr(self.eval(base)?, name),
            Expr::Index(base, index) => {
                let index = self.eval(index).and_then(|i| self.json(i))?;
                match self.eval(base)? {
                    Py::Json(v) => expr::subscript(&v, &index).map(Py::Json),
                    other => Err(PyError::Type(format!(
                        "'{}' object is not subscriptable",
                        type_of(&other)
                    ))),
                }
            }
            Expr::Method(base, name, args) => self.method(self.eval(base)?, name, args),
            Expr::Call(name, args) if name == "len" => {
                if args.len() != 1 {
                    return Err(PyError::Type(format!(
                        "len() takes exactly one argument ({} given)",
                        args.len()
                    )));
                }
                let n = match self.eval(&args[0])? {
                    Py::Json(Value::String(s)) => s.chars().count(),
                    Py::Json(Value::Array(a)) | Py::View(_, a) => a.len(),
                    Py::Json(Value::Object(m)) => m.len(),
                    Py::Items(i) => i.len(),
                    other => {
                        return Err(PyError::Type(format!(
                            "object of type '{}' has no len()",
                            type_of(&other)
                        )));
                    }
                };
                Ok(Py::Json(Value::from(n)))
            }
            Expr::Call(name, _) => Err(PyError::Name(name.clone())),
        }
    }

    fn attr(&self, base: Py, name: &str) -> Result<Py, PyError> {
        let found = match (&base, name) {
            (Py::Task, "name") | (Py::Result, "task_name") => {
                Some(Value::String(self.name.clone()))
            }
            (Py::Task, "action") => Some(Value::String(self.action.clone())),
            (Py::Task, "args") => Some(Value::Object(self.args.clone())),
            (Py::Host, "name") => Some(Value::String(self.host.clone())),
            (Py::Result, "_return_data") => Some(Value::Object(self.result.clone())),
            _ => None,
        };
        found.map(Py::Json).ok_or_else(|| {
            PyError::Attribute(format!(
                "'{}' object has no attribute '{name}'",
                type_of(&base)
            ))
        })
    }

    fn method(&self, base: Py, name: &str, args: &[Expr]) -> Result<Py, PyError> {
        let Py::Json(Value::Object(map)) = &base else {
            return Err(PyError::Attribute(format!(
                "'{}' object has no attribute '{name}'",
                type_of(&base)
            )));
        };
        let args: Vec<Value> = args
            .iter()
            .map(|a| self.eval(a).and_then(|v| self.json(v)))
            .collect::<Result<_, _>>()?;
        let none = |view: Py| {
            if args.is_empty() {
                Ok(view)
            } else {
                Err(PyError::Type(format!(
                    "dict.{name}() takes no arguments ({} given)",
                    args.len()
                )))
            }
        };
        match name {
            "keys" => none(Py::View(
                "dict_keys",
                map.keys().cloned().map(Value::String).collect(),
            )),
            "values" => none(Py::View("dict_values", map.values().cloned().collect())),
            "items" => none(Py::Items(
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            )),
            "get" if (1..=2).contains(&args.len()) => Ok(Py::Json(
                args[0]
                    .as_str()
                    .and_then(|k| map.get(k))
                    .or(args.get(1))
                    .cloned()
                    .unwrap_or(Value::Null),
            )),
            "get" => Err(PyError::Type(format!(
                "get expected at most 2 arguments, got {}",
                args.len()
            ))),
            _ => Err(PyError::Attribute(format!(
                "'dict' object has no attribute '{name}'"
            ))),
        }
    }

    /// `target = value`, for a name, `task.args` and a subscript of `task.args` or `task_vars`.
    fn assign(&mut self, target: &Expr, value: Value) -> Result<(), PyError> {
        let mut keys = Vec::new();
        let mut root = target;
        while let Expr::Index(base, index) = root {
            keys.push(self.eval(index).and_then(|i| self.json(i))?);
            root = base;
        }
        keys.reverse();
        match root {
            Expr::Name(name) if keys.is_empty() => {
                self.locals.insert(name.clone(), value);
                Ok(())
            }
            Expr::Name(name) if self.locals.contains_key(name) => {
                set(&mut self.locals[name.as_str()], &keys, value)
            }
            Expr::Attr(base, attr) if **base == Expr::Name("task".into()) && attr == "args" => {
                let mut args = Value::Object(std::mem::take(&mut self.args));
                let done = if keys.is_empty() {
                    if value.is_object() {
                        args = value;
                        Ok(())
                    } else {
                        Err(PyError::Type("task.args must be a dict".to_string()))
                    }
                } else {
                    set(&mut args, &keys, value)
                };
                if let Value::Object(map) = args {
                    self.args = map;
                }
                done
            }
            Expr::Name(name) if name == "task_vars" && !keys.is_empty() => {
                let Value::String(top) = &keys[0] else {
                    return Err(PyError::Key(keys[0].clone()));
                };
                if keys.len() == 1 {
                    self.vars.insert(top.clone(), value);
                    return Ok(());
                }
                let mut current = self
                    .task_vars()
                    .remove(top)
                    .ok_or_else(|| PyError::Key(keys[0].clone()))?;
                set(&mut current, &keys[1..], value)?;
                // A value that came from a managed host stays data after an edit inside it.
                if self.vars.untrusted.contains(top) {
                    self.vars.insert_untrusted(top.clone(), current);
                } else {
                    self.vars.insert(top.clone(), current);
                }
                Ok(())
            }
            other => match self.eval(other)? {
                Py::Json(_) if keys.is_empty() => Err(PyError::Syntax(1)),
                base => Err(PyError::Type(format!(
                    "'{}' object does not support item assignment",
                    type_of(&base)
                ))),
            },
        }
    }

    fn pformat(&self, value: &Py) -> String {
        match value {
            Py::Json(v) => expr::pformat(v),
            other => self.repr(other),
        }
    }

    fn repr(&self, value: &Py) -> String {
        let list = |items: Vec<String>| items.join(", ");
        match value {
            Py::Json(v) => expr::repr(v, false),
            Py::Task => format!("TASK: {}", self.title),
            Py::Host => self.host.clone(),
            Py::Result => format!("<TaskResult {} {}>", self.host, self.name),
            Py::View(kind, items) => format!(
                "{kind}([{}])",
                list(items.iter().map(|v| expr::repr(v, false)).collect())
            ),
            Py::Items(items) => format!(
                "dict_items([{}])",
                list(
                    items
                        .iter()
                        .map(|(k, v)| format!(
                            "({}, {})",
                            expr::repr(&Value::String(k.clone()), false),
                            expr::repr(v, false)
                        ))
                        .collect()
                )
            ),
        }
    }
}

/// `type(value).__name__`, with the reference's class names for its objects.
fn type_of(value: &Py) -> &'static str {
    match value {
        Py::Json(v) => expr::type_name(v),
        Py::Task => "Task",
        Py::Host => "Host",
        Py::Result => "_RawTaskResult",
        Py::View(kind, _) => kind,
        Py::Items(_) => "dict_items",
    }
}

/// `target[keys...] = value` inside a JSON value.
fn set(target: &mut Value, keys: &[Value], value: Value) -> Result<(), PyError> {
    let Some((last, path)) = keys.split_last() else {
        *target = value;
        return Ok(());
    };
    let mut at = target;
    for key in path {
        let found = match (&mut *at, key) {
            (Value::Object(map), Value::String(k)) => map.get_mut(k),
            (Value::Array(items), Value::Number(n)) => n
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| items.get_mut(i)),
            _ => None,
        };
        at = found.ok_or_else(|| PyError::Key(key.clone()))?;
    }
    match (at, last) {
        (Value::Object(map), Value::String(k)) => {
            map.insert(k.clone(), value);
            Ok(())
        }
        (Value::Array(items), Value::Number(n)) => {
            let slot = n
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| items.get_mut(i))
                .ok_or(PyError::Index)?;
            *slot = value;
            Ok(())
        }
        (other, _) => Err(PyError::Type(format!(
            "'{}' object does not support item assignment",
            expr::type_name(other)
        ))),
    }
}

/// `help`, and `help <command>`, as `cmd.Cmd.do_help` prints them.
fn help(arg: &str, out: &mut Renderer) {
    if arg.is_empty() {
        let header = "Documented commands (type help <topic>):";
        out.display("");
        out.display(header);
        out.display(&"=".repeat(header.len()));
        out.display(&COMMANDS.map(|(name, _)| name).join("  "));
        out.display("");
        return;
    }
    match COMMANDS.iter().find(|(name, _)| *name == arg) {
        Some((_, doc)) => out.display(doc),
        None => out.display(&format!("*** No help on {arg}")),
    }
}

#[cfg(test)]
pub(super) fn session_for_tests(host: &str, name: &str) -> Session {
    Session {
        host: host.to_string(),
        title: name.to_string(),
        name: name.to_string(),
        action: "command".to_string(),
        raw_args: Map::new(),
        args: Map::new(),
        ran: Map::new(),
        vars: HostVars::default(),
        result: Map::new(),
        templar: Arc::new(Templar::new(std::path::PathBuf::from("."))),
        locals: Map::new(),
        last: String::new(),
    }
}
