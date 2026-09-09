// SPDX-License-Identifier: GPL-3.0-or-later
//! Jinja2 templating the way ansible-core 2.19 does it: strict about undefined variables, and a
//! template that is one expression yields that expression's value, not its text.

use std::fmt;
use std::path::PathBuf;

use minijinja::{Environment, ErrorKind, UndefinedBehavior};
use serde_json::{Map, Value};

mod filters;

/// Prefix of every error raised because a variable had no value, the one template failure
/// Ansible treats as recoverable in places such as `vars_files`.
const UNDEFINED: &str = "The task includes an option with an undefined variable.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError(pub String);

impl TemplateError {
    /// Whether the render failed on a variable that had no value, as opposed to a syntax error,
    /// an unknown filter or an illegal operation.
    pub fn is_undefined(&self) -> bool {
        self.0.starts_with(UNDEFINED)
    }
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TemplateError {}

pub struct Templar {
    env: Environment<'static>,
}

impl Templar {
    /// `base_dir` anchors relative paths in lookups such as `lookup('file', ...)`.
    pub fn new(base_dir: PathBuf) -> Self {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        filters::register(&mut env, base_dir);
        Self { env }
    }

    pub fn is_template(text: &str) -> bool {
        text.contains("{{") || text.contains("{%")
    }

    /// Renders a string. Exactly one `{{ expression }}` gives the expression's value with its
    /// type; text around or between expressions gives a string.
    pub fn render(&self, text: &str, vars: &Map<String, Value>) -> Result<Value, TemplateError> {
        let mut value = self.render_once(text, vars)?;
        // A variable can hold a template of its own, so Ansible renders a result again while it
        // still carries a marker. Three further passes: a chain longer than that is a loop, and
        // an unchanged result ends it earlier.
        for _ in 0..3 {
            let Value::String(text) = &value else { break };
            if !Self::is_template(text) {
                break;
            }
            let next = self.render_once(text, vars)?;
            if next == value {
                break;
            }
            value = next;
        }
        Ok(value)
    }

    fn render_once(&self, text: &str, vars: &Map<String, Value>) -> Result<Value, TemplateError> {
        if !Self::is_template(text) {
            return Ok(Value::String(text.to_string()));
        }
        if let Some(expr) = single_expression(text) {
            return self.evaluate(expr, vars);
        }
        self.env
            .render_str(text, minijinja::Value::from_serialize(vars))
            .map(Value::String)
            .map_err(convert_error)
    }

    /// Templates every string inside a value. Mapping keys are left alone.
    pub fn render_value(
        &self,
        value: &Value,
        vars: &Map<String, Value>,
    ) -> Result<Value, TemplateError> {
        Ok(match value {
            Value::String(s) => self.render(s, vars)?,
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|v| self.render_value(v, vars))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(map) => {
                let mut out = Map::new();
                for (k, v) in map {
                    out.insert(k.clone(), self.render_value(v, vars)?);
                }
                Value::Object(out)
            }
            other => other.clone(),
        })
    }

    /// Evaluates one expression and returns its value with its type.
    pub fn evaluate(&self, expr: &str, vars: &Map<String, Value>) -> Result<Value, TemplateError> {
        let compiled = self.env.compile_expression(expr).map_err(convert_error)?;
        let value = compiled
            .eval(minijinja::Value::from_serialize(vars))
            .map_err(convert_error)?;
        // Strict mode only raises on operations that force an undefined value (printing,
        // comparing, ...); an attribute lookup that never gets used stays a lazy Undefined that
        // serde_json would otherwise turn into `null`. Force the same error here.
        if value.is_undefined() {
            return Err(convert_error(minijinja::Error::from(
                ErrorKind::UndefinedError,
            )));
        }
        serde_json::to_value(&value)
            .map_err(|e| TemplateError(format!("cannot convert template result: {e}")))
    }

    /// Templates the string values of a variable map against the map itself, a few passes, until
    /// nothing changes. Best effort: a value that fails to render is left as written, so the
    /// error surfaces where the value is used, as it does in Ansible.
    pub fn resolve_vars(&self, vars: &Map<String, Value>) -> Map<String, Value> {
        // Most maps hold no template at all, and `hostvars` is left alone below: walking them
        // once is far cheaper than the two clones a pass costs.
        if !vars
            .iter()
            .any(|(k, v)| k != "hostvars" && holds_template(v))
        {
            return vars.clone();
        }
        let mut current = vars.clone();
        for _ in 0..5 {
            let mut next = current.clone();
            let mut changed = false;
            for (k, v) in &current {
                if k == "hostvars" {
                    continue;
                }
                let rendered = self.render_value_lenient(v, &current);
                if rendered != *v {
                    changed = true;
                    next.insert(k.clone(), rendered);
                }
            }
            current = next;
            if !changed {
                break;
            }
        }
        current
    }

    fn render_value_lenient(&self, value: &Value, vars: &Map<String, Value>) -> Value {
        match value {
            Value::String(s) if Self::is_template(s) => {
                self.render(s, vars).unwrap_or_else(|_| value.clone())
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|v| self.render_value_lenient(v, vars))
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.render_value_lenient(v, vars)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// A `when` clause. Ansible accepts `{{ }}` around it with a warning; the result must be a
    /// boolean, anything else is an error in the reference release.
    pub fn condition(&self, expr: &str, vars: &Map<String, Value>) -> Result<bool, TemplateError> {
        let expr = single_expression(expr).unwrap_or(expr.trim());
        match self.evaluate(expr, vars)? {
            Value::Bool(b) => Ok(b),
            other => Err(TemplateError(format!(
                "Conditional result was {other} of type {}, which evaluates to {}. Conditionals must have a boolean result.",
                type_name(&other),
                if truthy(&other) { "True" } else { "False" }
            ))),
        }
    }
}

/// Whether any string inside a value still carries a template marker.
fn holds_template(value: &Value) -> bool {
    match value {
        Value::String(s) => Templar::is_template(s),
        Value::Array(items) => items.iter().any(holds_template),
        Value::Object(map) => map.values().any(holds_template),
        _ => false,
    }
}

/// `{{ expr }}` and nothing else: no text around, no second expression, no statement.
fn single_expression(text: &str) -> Option<&str> {
    let t = text.trim();
    let inner = t.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") || inner.contains("{%") {
        return None;
    }
    Some(inner.trim())
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Python truthiness, for messages and for filters that need it.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn convert_error(err: minijinja::Error) -> TemplateError {
    let text = err.to_string();
    if err.kind() == ErrorKind::UndefinedError {
        return TemplateError(format!("{UNDEFINED} The error was: {text}"));
    }
    TemplateError(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn a_single_expression_keeps_its_type_and_text_renders_to_a_string() {
        let t = Templar::new(std::env::temp_dir());
        assert_eq!(t.render("{{ 1 + 1 }}", &Map::new()).unwrap(), json!(2));
        assert_eq!(
            t.render("{{ flag }}", &vars(json!({"flag": true})))
                .unwrap(),
            json!(true)
        );
        assert_eq!(
            t.render("{{ items }}", &vars(json!({"items": [1, 2]})))
                .unwrap(),
            json!([1, 2])
        );
        assert_eq!(
            t.render("n={{ 1 + 1 }}", &Map::new()).unwrap(),
            json!("n=2")
        );
        assert_eq!(
            t.render("{{ a }}{{ b }}", &vars(json!({"a": 1, "b": 2})))
                .unwrap(),
            json!("12")
        );
        assert_eq!(t.render("plain", &Map::new()).unwrap(), json!("plain"));
    }

    #[test]
    fn undefined_variables_are_errors_with_ansible_wording() {
        let t = Templar::new(std::env::temp_dir());
        let err = t.render("{{ missing }}", &Map::new()).unwrap_err();
        assert!(
            err.0
                .starts_with("The task includes an option with an undefined variable."),
            "{err}"
        );
        assert!(
            t.render("{{ conf.missing }}", &vars(json!({"conf": {}})))
                .is_err()
        );
    }

    #[test]
    fn values_are_templated_recursively_but_keys_are_not() {
        let t = Templar::new(std::env::temp_dir());
        let v = t
            .render_value(
                &json!({"{{ k }}": ["{{ x }}", {"n": "{{ x + 1 }}"}], "lit": 3}),
                &vars(json!({"k": "key", "x": 1})),
            )
            .unwrap();
        assert_eq!(v, json!({"{{ k }}": [1, {"n": 2}], "lit": 3}));
    }

    #[test]
    fn resolve_vars_chains_references_and_leaves_broken_ones_alone() {
        let t = Templar::new(std::env::temp_dir());
        let resolved = t.resolve_vars(&vars(
            json!({"a": "{{ b }}", "b": "{{ c }}!", "c": "x", "bad": "{{ missing }}"}),
        ));
        assert_eq!(resolved["a"], json!("x!"));
        assert_eq!(resolved["c"], json!("x"));
        assert_eq!(resolved["bad"], json!("{{ missing }}"));
    }

    #[test]
    fn conditions_need_a_boolean_and_accept_stray_braces() {
        let t = Templar::new(std::env::temp_dir());
        assert!(t.condition("x > 3", &vars(json!({"x": 5}))).unwrap());
        assert!(!t.condition("x > 3", &vars(json!({"x": 1}))).unwrap());
        assert!(t.condition("{{ x > 3 }}", &vars(json!({"x": 5}))).unwrap());
        let err = t.condition("'yes'", &Map::new()).unwrap_err();
        assert!(
            err.0.contains("Conditionals must have a boolean result"),
            "{err}"
        );
        assert!(t.condition("missing > 3", &Map::new()).is_err());
    }
}
