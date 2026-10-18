// SPDX-License-Identifier: GPL-3.0-or-later
//! The part of Python the debugger prompt evaluates: names, literals, attributes, subscripts,
//! `len()`, the `keys()`, `values()` and `items()` of a mapping, and assignment to a subscript.
//! Values print as Python's `repr` and `pprint.pformat` print them.

use serde_json::{Map, Value};

use crate::playbook::python_repr;

/// One parsed expression.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Expr {
    Name(String),
    Literal(Value),
    List(Vec<Expr>),
    Dict(Vec<(Expr, Expr)>),
    Attr(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    /// `value.method(arguments)`.
    Method(Box<Expr>, String, Vec<Expr>),
    /// `function(arguments)`.
    Call(String, Vec<Expr>),
}

/// One line typed at the prompt that is not a command.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Statement {
    Expr(Expr),
    Assign(Expr, Expr),
}

/// A Python exception, as the prompt prints it: `***KeyError:KeyError('x')`.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum PyError {
    Name(String),
    Key(Value),
    Index,
    Type(String),
    Attribute(String),
    /// Where the parse stopped, as a 1-based column.
    Syntax(usize),
}

impl PyError {
    /// The line the reference prints for this exception, for `text` evaluated (`p`, file
    /// `<string>`) or executed (any other line, file `<stdin>`).
    // A syntax error's position is where this parser stopped, which is not always the column
    // CPython reports.
    pub(super) fn line(&self, text: &str, evaluated: bool) -> String {
        let s = |m: &str| python_repr(&Value::String(m.to_string()));
        let (kind, args) = match self {
            Self::Name(name) => ("NameError", s(&format!("name '{name}' is not defined"))),
            Self::Key(key) => ("KeyError", python_repr(key)),
            Self::Index => ("IndexError", s("list index out of range")),
            Self::Type(m) => ("TypeError", s(m)),
            Self::Attribute(m) => ("AttributeError", s(m)),
            Self::Syntax(col) => {
                let (file, source) = if evaluated {
                    ("<string>", text.to_string())
                } else {
                    ("<stdin>", format!("{text}\n"))
                };
                (
                    "SyntaxError",
                    format!(
                        "'invalid syntax', ('{file}', 1, {col}, {}, 1, {})",
                        s(&source),
                        col + 1
                    ),
                )
            }
        };
        format!("***{kind}:{kind}({args})")
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Literal(Value),
    Punct(char),
}

fn tokens(text: &str) -> Result<Vec<(usize, Token)>, PyError> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let at = i + 1;
        if c.is_whitespace() {
            i += 1;
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            out.push((
                at,
                match word.as_str() {
                    "True" => Token::Literal(Value::Bool(true)),
                    "False" => Token::Literal(Value::Bool(false)),
                    "None" => Token::Literal(Value::Null),
                    _ => Token::Ident(word),
                },
            ));
        } else if c.is_ascii_digit()
            || (c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let number: String = chars[start..i].iter().collect();
            let value = number
                .parse::<i64>()
                .map(Value::from)
                .or_else(|_| number.parse::<f64>().map(Value::from))
                .map_err(|_| PyError::Syntax(at))?;
            out.push((at, Token::Literal(value)));
        } else if c == '\'' || c == '"' {
            i += 1;
            let mut s = String::new();
            loop {
                match chars.get(i) {
                    None => return Err(PyError::Syntax(at)),
                    Some(&q) if q == c => break,
                    Some('\\') => {
                        i += 1;
                        match chars.get(i) {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('r') => s.push('\r'),
                            Some(&other) => s.push(other),
                            None => return Err(PyError::Syntax(at)),
                        }
                    }
                    Some(&other) => s.push(other),
                }
                i += 1;
            }
            i += 1;
            out.push((at, Token::Literal(Value::String(s))));
        } else if ".[](){},:=".contains(c) {
            out.push((at, Token::Punct(c)));
            i += 1;
        } else {
            return Err(PyError::Syntax(at));
        }
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<(usize, Token)>,
    pos: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|(_, t)| t)
    }

    fn here(&self) -> usize {
        self.tokens.get(self.pos).map_or(self.end, |(at, _)| *at)
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(&Token::Punct(c)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: char) -> Result<(), PyError> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(PyError::Syntax(self.here()))
        }
    }

    /// The comma-separated expressions up to `close`, which is eaten.
    fn list(&mut self, close: char) -> Result<Vec<Expr>, PyError> {
        let mut items = Vec::new();
        while !self.eat(close) {
            items.push(self.expr()?);
            if !self.eat(',') {
                self.expect(close)?;
                break;
            }
        }
        Ok(items)
    }

    fn primary(&mut self) -> Result<Expr, PyError> {
        let at = self.here();
        let Some((_, token)) = self.tokens.get(self.pos).cloned() else {
            return Err(PyError::Syntax(at));
        };
        self.pos += 1;
        match token {
            Token::Literal(v) => Ok(Expr::Literal(v)),
            Token::Ident(name) if self.eat('(') => Ok(Expr::Call(name, self.list(')')?)),
            Token::Ident(name) => Ok(Expr::Name(name)),
            Token::Punct('(') => {
                let inner = self.expr()?;
                self.expect(')')?;
                Ok(inner)
            }
            Token::Punct('[') => Ok(Expr::List(self.list(']')?)),
            Token::Punct('{') => {
                let mut pairs = Vec::new();
                while !self.eat('}') {
                    let key = self.expr()?;
                    self.expect(':')?;
                    pairs.push((key, self.expr()?));
                    if !self.eat(',') {
                        self.expect('}')?;
                        break;
                    }
                }
                Ok(Expr::Dict(pairs))
            }
            Token::Punct(_) => Err(PyError::Syntax(at)),
        }
    }

    fn expr(&mut self) -> Result<Expr, PyError> {
        let mut e = self.primary()?;
        loop {
            if self.eat('.') {
                let Some(Token::Ident(name)) = self.peek().cloned() else {
                    return Err(PyError::Syntax(self.here()));
                };
                self.pos += 1;
                e = if self.eat('(') {
                    Expr::Method(Box::new(e), name, self.list(')')?)
                } else {
                    Expr::Attr(Box::new(e), name)
                };
            } else if self.eat('[') {
                let index = self.expr()?;
                self.expect(']')?;
                e = Expr::Index(Box::new(e), Box::new(index));
            } else {
                return Ok(e);
            }
        }
    }
}

fn parser(text: &str) -> Result<Parser, PyError> {
    Ok(Parser {
        tokens: tokens(text)?,
        pos: 0,
        end: text.chars().count() + 1,
    })
}

/// `text` as one expression, which is what `p` evaluates.
pub(super) fn expression(text: &str) -> Result<Expr, PyError> {
    let mut p = parser(text)?;
    let e = p.expr()?;
    if p.peek().is_some() {
        return Err(PyError::Syntax(p.here()));
    }
    Ok(e)
}

/// `text` as one statement: an expression, or an assignment to a subscript or an attribute.
pub(super) fn statement(text: &str) -> Result<Statement, PyError> {
    let mut p = parser(text)?;
    let e = p.expr()?;
    if p.peek().is_none() {
        return Ok(Statement::Expr(e));
    }
    let at = p.here();
    p.expect('=')?;
    if matches!(
        e,
        Expr::Literal(_) | Expr::List(_) | Expr::Dict(_) | Expr::Call(..) | Expr::Method(..)
    ) {
        return Err(PyError::Syntax(at));
    }
    let value = p.expr()?;
    if p.peek().is_some() {
        return Err(PyError::Syntax(p.here()));
    }
    Ok(Statement::Assign(e, value))
}

/// Python's `type(value).__name__` for a JSON value.
pub(super) fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `value[index]` for a JSON value.
pub(super) fn subscript(value: &Value, index: &Value) -> Result<Value, PyError> {
    match (value, index) {
        (Value::Object(map), Value::String(k)) => map
            .get(k)
            .cloned()
            .ok_or_else(|| PyError::Key(index.clone())),
        (Value::Object(_), _) => Err(PyError::Key(index.clone())),
        (Value::Array(items), Value::Number(n)) if n.is_i64() => {
            let n = n.as_i64().unwrap_or_default();
            let at = if n < 0 {
                items.len().checked_sub(n.unsigned_abs() as usize)
            } else {
                usize::try_from(n).ok()
            };
            at.and_then(|i| items.get(i)).cloned().ok_or(PyError::Index)
        }
        (Value::Array(_), other) => Err(PyError::Type(format!(
            "list indices must be integers or slices, not {}",
            type_name(other)
        ))),
        (Value::String(s), Value::Number(n)) if n.is_i64() => {
            let chars: Vec<char> = s.chars().collect();
            let n = n.as_i64().unwrap_or_default();
            let at = if n < 0 {
                chars.len().checked_sub(n.unsigned_abs() as usize)
            } else {
                usize::try_from(n).ok()
            };
            at.and_then(|i| chars.get(i))
                .map(|c| Value::String(c.to_string()))
                .ok_or_else(|| PyError::Type("string index out of range".to_string()))
        }
        (other, _) => Err(PyError::Type(format!(
            "'{}' object is not subscriptable",
            type_name(other)
        ))),
    }
}

/// Python's `repr`, with the keys of a mapping sorted when `sorted` (what `pprint` does).
pub(super) fn repr(value: &Value, sorted: bool) -> String {
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|v| repr(v, sorted))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            entries(map, sorted)
                .map(|(k, v)| format!(
                    "{}: {}",
                    python_repr(&Value::String(k.clone())),
                    repr(v, sorted)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => python_repr(other),
    }
}

fn entries(map: &Map<String, Value>, sorted: bool) -> impl Iterator<Item = (&String, &Value)> {
    let mut all: Vec<_> = map.iter().collect();
    if sorted {
        all.sort_by(|a, b| a.0.cmp(b.0));
    }
    all.into_iter()
}

/// `pprint.pformat(value)`: the repr when it fits in 80 columns, else one entry per line.
// A string longer than the line is kept whole, where pprint cuts it into adjacent literals.
pub(super) fn pformat(value: &Value) -> String {
    pformat_at(value, 0, 0)
}

fn pformat_at(value: &Value, indent: usize, allowance: usize) -> String {
    const WIDTH: usize = 80;
    let flat = repr(value, true);
    if flat.chars().count() + indent + allowance <= WIDTH {
        return flat;
    }
    let pad = format!(",\n{}", " ".repeat(indent + 1));
    match value {
        Value::Object(map) if !map.is_empty() => {
            let all: Vec<_> = entries(map, true).collect();
            let last = all.len() - 1;
            let body: Vec<String> = all
                .iter()
                .enumerate()
                .map(|(i, (k, v))| {
                    let key = python_repr(&Value::String((*k).clone()));
                    let room = if i == last { allowance + 1 } else { 1 };
                    let inner = pformat_at(v, indent + 1 + key.chars().count() + 2, room);
                    format!("{key}: {inner}")
                })
                .collect();
            format!("{{{}}}", body.join(&pad))
        }
        Value::Array(items) if !items.is_empty() => {
            let last = items.len() - 1;
            let body: Vec<String> = items
                .iter()
                .enumerate()
                .map(|(i, v)| pformat_at(v, indent + 1, if i == last { allowance + 1 } else { 1 }))
                .collect();
            format!("[{}]", body.join(&pad))
        }
        _ => flat,
    }
}
