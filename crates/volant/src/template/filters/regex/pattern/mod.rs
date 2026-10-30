// SPDX-License-Identifier: GPL-3.0-or-later
//! A Python `re` pattern, rewritten for the `regex` crate, or refused with a reason when the
//! crate has no counterpart for what the pattern says.

use std::fmt::Write as _;

/// Why a pattern has no Rust counterpart.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Failure {
    /// Python refuses the pattern too.
    Invalid(String),
    /// Python accepts the pattern, and the Rust engine cannot run it.
    Unsupported(String),
}

/// A pattern in the syntax of the `regex` crate.
pub(super) struct Translated {
    /// The pattern with `$` as the `regex` crate reads it: the end of the text, or the end of a
    /// line inside a `(?m:` span.
    pub(super) strict: String,
    /// The same pattern with each `$` that Python reads as "the end, or before a final newline"
    /// written as `(?m:$)`. It is exact on a text with no newline except a final one.
    pub(super) loose: Option<String>,
    /// A lazy repeat or an empty branch is present. Python may then prefer a longer match that
    /// the `regex` crate cannot be asked for.
    pub(super) prefers_empty: bool,
}

fn invalid<T>(why: impl Into<String>, at: usize) -> Result<T, Failure> {
    Err(Failure::Invalid(format!("{} at position {at}", why.into())))
}

fn unsupported<T>(what: &str) -> Result<T, Failure> {
    Err(Failure::Unsupported(what.to_string()))
}

/// What the last item of the branch being read is, for Python's repeat rules.
#[derive(Clone, Copy, PartialEq)]
enum Last {
    /// Nothing yet: the start, `(` or `|`.
    Nothing,
    /// `^`, `$`, `\A`, `\Z`, `\b` or `\B`, which Python refuses to repeat.
    Assertion,
    /// Something a repeat applies to.
    Item,
    /// A repeat, which Python refuses to repeat again.
    Repeat,
}

/// The one thing in a set that a range can start from.
#[derive(Clone, Copy, PartialEq)]
enum Prev {
    None,
    Char(char),
    Class,
}

struct Scanner {
    src: Vec<char>,
    at: usize,
    out: String,
    /// Where each `$` that depends on the text stands in `out`.
    dollars: Vec<usize>,
    last: Last,
    verbose: bool,
    multiline: bool,
    /// The multiline state to go back to at each `)`.
    scopes: Vec<Scope>,
    /// Only global flag groups have been read so far.
    at_start: bool,
    prefers_empty: bool,
    /// The branch being read has no item yet.
    branch_empty: bool,
}

struct Scope {
    multiline: bool,
    open_at: usize,
}

pub(super) fn translate(pattern: &str, multiline: bool) -> Result<Translated, Failure> {
    let mut s = Scanner {
        src: pattern.chars().collect(),
        at: 0,
        out: String::new(),
        dollars: Vec::new(),
        last: Last::Nothing,
        verbose: false,
        multiline,
        scopes: vec![Scope {
            multiline,
            open_at: 0,
        }],
        at_start: true,
        prefers_empty: false,
        branch_empty: true,
    };
    s.run()?;
    let loose = (!s.dollars.is_empty()).then(|| {
        let mut loose = String::with_capacity(s.out.len() + 8 * s.dollars.len());
        let mut from = 0;
        for &at in &s.dollars {
            loose.push_str(&s.out[from..at]);
            loose.push_str("(?m:$)");
            from = at + 1;
        }
        loose.push_str(&s.out[from..]);
        loose
    });
    Ok(Translated {
        strict: s.out,
        loose,
        prefers_empty: s.prefers_empty,
    })
}

fn is_name_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

pub(super) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(is_name_start) && chars.all(|c| c == '_' || c.is_alphanumeric())
}

impl Scanner {
    fn peek(&self) -> Option<char> {
        self.src.get(self.at).copied()
    }

    fn item(&mut self) {
        self.last = Last::Item;
        self.branch_empty = false;
        self.at_start = false;
    }

    fn assertion(&mut self) {
        self.last = Last::Assertion;
        self.branch_empty = false;
        self.at_start = false;
    }

    fn run(&mut self) -> Result<(), Failure> {
        while let Some(c) = self.peek() {
            self.at += 1;
            if self.verbose && matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}') {
                continue;
            }
            if self.verbose && c == '#' {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.at += 1;
                }
                continue;
            }
            match c {
                '\\' => self.escape()?,
                '(' => self.group()?,
                ')' => {
                    if self.scopes.len() == 1 {
                        return invalid("unbalanced parenthesis", self.at - 1);
                    }
                    let scope = self.scopes.pop().expect("a scope is open");
                    self.multiline = scope.multiline;
                    self.out.push(')');
                    self.item();
                }
                '|' => {
                    if self.branch_empty {
                        self.prefers_empty = true;
                    }
                    self.out.push('|');
                    self.last = Last::Nothing;
                    self.branch_empty = true;
                    self.at_start = false;
                }
                '^' => {
                    self.out.push('^');
                    self.assertion();
                }
                '$' => {
                    if !self.multiline {
                        self.dollars.push(self.out.len());
                    }
                    self.out.push('$');
                    self.assertion();
                }
                '.' => {
                    self.out.push('.');
                    self.item();
                }
                '[' => self.set()?,
                '*' | '+' | '?' => self.repeat(c.to_string())?,
                '{' => {
                    if let Some(repeat) = self.braces() {
                        self.repeat(repeat)?;
                    } else {
                        self.out.push_str("\\{");
                        self.item();
                    }
                }
                '}' => {
                    self.out.push_str("\\}");
                    self.item();
                }
                c => {
                    push_literal(&mut self.out, c);
                    self.item();
                }
            }
        }
        if self.scopes.len() > 1 {
            let scope = self.scopes.last().expect("a scope is open");
            return invalid("missing ), unterminated subpattern", scope.open_at);
        }
        Ok(())
    }

    fn repeat(&mut self, text: String) -> Result<(), Failure> {
        match self.last {
            Last::Item => {}
            Last::Repeat => return invalid("multiple repeat", self.at - 1),
            _ => return invalid("nothing to repeat", self.at - 1),
        }
        self.out.push_str(&text);
        match self.peek() {
            Some('?') => {
                self.at += 1;
                self.out.push('?');
                self.prefers_empty = true;
            }
            Some('+') => return unsupported("a possessive quantifier"),
            _ => {}
        }
        self.last = Last::Repeat;
        Ok(())
    }

    /// `{m,n}` and its shorter forms, from just after the brace, as Rust writes them. `None`
    /// when Python reads the brace as itself.
    fn braces(&mut self) -> Option<String> {
        let mut at = self.at;
        if self.src.get(at) == Some(&'}') {
            return None;
        }
        let digits = |at: &mut usize| {
            let start = *at;
            while self.src.get(*at).is_some_and(char::is_ascii_digit) {
                *at += 1;
            }
            self.src[start..*at].iter().collect::<String>()
        };
        let low = digits(&mut at);
        let high = if self.src.get(at) == Some(&',') {
            at += 1;
            Some(digits(&mut at))
        } else {
            None
        };
        if self.src.get(at) != Some(&'}') {
            return None;
        }
        self.at = at + 1;
        let low = if low.is_empty() { "0".to_string() } else { low };
        Some(match high {
            None => format!("{{{low}}}"),
            Some(high) => format!("{{{low},{high}}}"),
        })
    }

    fn group(&mut self) -> Result<(), Failure> {
        let open_at = self.at - 1;
        if self.peek() != Some('?') {
            self.open(open_at);
            self.out.push('(');
            return Ok(());
        }
        self.at += 1;
        let Some(c) = self.peek() else {
            return invalid("unexpected end of pattern", self.at);
        };
        self.at += 1;
        match c {
            ':' => {
                self.open(open_at);
                self.out.push_str("(?:");
            }
            'P' => match self.peek() {
                Some('<') => {
                    self.at += 1;
                    let start = self.at;
                    while self.peek().is_some_and(|c| c != '>') {
                        self.at += 1;
                    }
                    if self.peek().is_none() {
                        return invalid("missing >, unterminated name", start);
                    }
                    let name: String = self.src[start..self.at].iter().collect();
                    self.at += 1;
                    if name.is_empty() {
                        return invalid("missing group name", start);
                    }
                    if !is_identifier(&name) {
                        return invalid(format!("bad character in group name '{name}'"), start);
                    }
                    self.open(open_at);
                    let _ = write!(self.out, "(?P<{name}>");
                }
                Some('=') => {
                    self.at += 1;
                    let start = self.at;
                    while self.peek().is_some_and(|c| c != ')') {
                        self.at += 1;
                    }
                    if self.peek().is_none() {
                        return invalid("missing ), unterminated name", start);
                    }
                    let name: String = self.src[start..self.at].iter().collect();
                    self.at += 1;
                    if !is_identifier(&name) {
                        return invalid(format!("bad character in group name '{name}'"), start);
                    }
                    let _ = write!(self.out, "\\k<{name}>");
                    self.item();
                }
                Some('>') => return unsupported("a recursive group, '(?P>name)'"),
                _ => return invalid("unknown extension ?P", self.at),
            },
            '#' => {
                while self.peek().is_some_and(|c| c != ')') {
                    self.at += 1;
                }
                if self.peek().is_none() {
                    return invalid("missing ), unterminated comment", open_at);
                }
                self.at += 1;
            }
            // Look-around and atomic groups, which the backtracking engine runs.
            '=' | '!' => {
                self.open(open_at);
                let _ = write!(self.out, "(?{c}");
            }
            '<' => match self.peek() {
                Some(kind @ ('=' | '!')) => {
                    self.at += 1;
                    self.open(open_at);
                    let _ = write!(self.out, "(?<{kind}");
                }
                _ => return invalid("unknown extension ?<", self.at),
            },
            '>' => {
                self.open(open_at);
                self.out.push_str("(?>");
            }
            '(' => return unsupported("a conditional group, '(?(id)yes|no)'"),
            c if c.is_ascii_alphabetic() || c == '-' => {
                self.at -= 1;
                self.flags(open_at)?;
            }
            c => return invalid(format!("unknown extension ?{c}"), self.at - 1),
        }
        Ok(())
    }

    fn open(&mut self, open_at: usize) {
        self.scopes.push(Scope {
            multiline: self.multiline,
            open_at,
        });
        self.last = Last::Nothing;
        self.branch_empty = true;
        self.at_start = false;
    }

    /// Flags after `(?`: a global group such as `(?i)`, which Python allows only at the start,
    /// or a scoped one such as `(?i-s:`.
    fn flags(&mut self, open_at: usize) -> Result<(), Failure> {
        let (mut on, mut off) = (String::new(), String::new());
        let mut negative = false;
        loop {
            let Some(c) = self.peek() else {
                return invalid("missing -, : or )", self.at);
            };
            self.at += 1;
            match c {
                '-' if !negative => negative = true,
                'i' | 'm' | 's' | 'x' | 'a' | 'u' | 'L' => {
                    if negative {
                        if matches!(c, 'a' | 'u' | 'L') {
                            return invalid(
                                "bad inline flags: cannot turn off flags 'a', 'u' and 'L'",
                                self.at - 1,
                            );
                        }
                        off.push(c);
                    } else {
                        on.push(c);
                    }
                }
                ')' if !negative => {
                    if !self.at_start {
                        return invalid("global flags not at the start of the expression", open_at);
                    }
                    self.check_flags(&on)?;
                    if on.contains('x') {
                        self.verbose = true;
                    }
                    if on.contains('m') {
                        self.multiline = true;
                    }
                    let kept: String = on
                        .chars()
                        .filter(|c| matches!(c, 'i' | 'm' | 's'))
                        .collect();
                    if !kept.is_empty() {
                        let _ = write!(self.out, "(?{kept})");
                    }
                    return Ok(());
                }
                ':' => {
                    if on.is_empty() && off.is_empty() {
                        return invalid("missing flag", self.at - 1);
                    }
                    self.check_flags(&on)?;
                    self.check_flags(&off)?;
                    if on.contains('x') || off.contains('x') {
                        return unsupported("the verbose flag in a group, '(?x:'");
                    }
                    let scoped: String = on
                        .chars()
                        .filter(|c| matches!(c, 'i' | 'm' | 's'))
                        .collect();
                    let removed: String = off.chars().collect();
                    let outer = self.multiline;
                    self.open(open_at);
                    if let Some(scope) = self.scopes.last_mut() {
                        scope.multiline = outer;
                    }
                    if scoped.contains('m') {
                        self.multiline = true;
                    }
                    if removed.contains('m') {
                        self.multiline = false;
                    }
                    let _ = write!(self.out, "(?{scoped}");
                    if !removed.is_empty() {
                        let _ = write!(self.out, "-{removed}");
                    }
                    self.out.push(':');
                    return Ok(());
                }
                c => return invalid(format!("unknown flag '{c}'"), self.at - 1),
            }
        }
    }

    fn check_flags(&self, flags: &str) -> Result<(), Failure> {
        if flags.contains('a') {
            return unsupported("the ASCII flag, '(?a)'");
        }
        if flags.contains('L') {
            return unsupported("the locale flag, '(?L)'");
        }
        Ok(())
    }
}

mod chars;
use chars::push_literal;
