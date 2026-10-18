// SPDX-License-Identifier: GPL-3.0-or-later
//! How a scalar is written: its text, and which styles it may take.

use std::sync::LazyLock;

use minijinja::Value;
use minijinja::value::ValueKind;

use crate::yaml::{PYYAML_FALSE, PYYAML_TRUE};

/// PyYAML's YAML 1.1 implicit resolvers other than the booleans: int, float, null, merge,
/// value and timestamp, in `resolver.py`'s spelling. A string one of them matches would be read
/// back as something else, so it is never written plain.
static RESOLVED: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"^(?:",
        r"[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+",
        r"|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+",
        r"|[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?",
        r"|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)",
        r"|~|null|Null|NULL|<<|=",
        r"|[0-9]{4}-[0-9]{2}-[0-9]{2}",
        r"|[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}(?:[Tt]|[ \t]+)[0-9]{1,2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]*)?",
        r"(?:[ \t]*(?:Z|[-+][0-9]{1,2}(?::[0-9]{2})?))?",
        r")$"
    ))
    .expect("static pattern")
});

/// Whether PyYAML would resolve this plain string to a type other than `str`: the empty
/// string is null.
pub(super) fn resolves_elsewhere(s: &str) -> bool {
    s.is_empty() || PYYAML_TRUE.contains(&s) || PYYAML_FALSE.contains(&s) || RESOLVED.is_match(s)
}

/// libyaml's `IS_BREAK`, which counts `\r` where `emitter.py` does not.
pub(super) fn is_break(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// libyaml's `IS_PRINTABLE`: the Basic Multilingual Plane's printable ranges only, so `\x85`
/// and every character above U+FFFF are not, unlike `emitter.py`.
pub(super) fn is_printable(c: char) -> bool {
    c == '\n'
        || ('\x20'..='\x7e').contains(&c)
        || ('\u{a0}'..='\u{d7ff}').contains(&c)
        || (('\u{e000}'..='\u{fffd}').contains(&c) && c != '\u{feff}')
}

pub(super) fn is_collection(v: &Value) -> bool {
    matches!(
        v.kind(),
        ValueKind::Seq | ValueKind::Map | ValueKind::Iterable
    )
}

/// A scalar's text as the representer writes it, and whether it may be written plain as far as
/// its type is concerned (`implicit[0]`): a string that YAML 1.1 would read back as something
/// else may not.
pub(super) fn scalar_text(v: &Value) -> (String, bool) {
    match v.kind() {
        ValueKind::Undefined | ValueKind::None => ("null".to_string(), true),
        ValueKind::Bool => (v.is_true().to_string(), true),
        ValueKind::Number if v.is_integer() => (v.to_string(), true),
        ValueKind::Number => (py_float(f64::try_from(v.clone()).unwrap_or(f64::NAN)), true),
        _ => {
            let s = v.as_str().map_or_else(|| v.to_string(), str::to_string);
            let implicit = !resolves_elsewhere(&s);
            (s, implicit)
        }
    }
}

/// Python's `repr` of a float, then PyYAML's `represent_float`: lower case, and `.0` before an
/// exponent that has no decimal point (`1e+17` would not read back as a float).
fn py_float(f: f64) -> String {
    if f.is_nan() {
        return ".nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { ".inf" } else { "-.inf" }.into();
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').expect("exponent form");
    let exp: i32 = exp.parse().expect("exponent");
    let text = if (-4..16).contains(&exp) {
        let s = f.to_string();
        if s.contains('.') { s } else { format!("{s}.0") }
    } else {
        format!(
            "{mantissa}e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    };
    if !text.contains('.') && text.contains('e') {
        text.replacen('e', ".0e", 1)
    } else {
        text
    }
}

/// `analyze_scalar`'s answer, reduced to the styles a scalar of plain data can take.
pub(super) struct Analysis {
    pub(super) empty: bool,
    pub(super) multiline: bool,
    pub(super) allow_flow_plain: bool,
    pub(super) allow_block_plain: bool,
    pub(super) allow_single_quoted: bool,
}

#[expect(clippy::too_many_lines)]
pub(super) fn analyze(s: &[char]) -> Analysis {
    if s.is_empty() {
        return Analysis {
            empty: true,
            multiline: false,
            allow_flow_plain: false,
            allow_block_plain: true,
            allow_single_quoted: true,
        };
    }
    let ws = |c: char| matches!(c, '\0' | ' ' | '\t' | '\r') || is_break(c);
    let (mut block_indicators, mut flow_indicators) = (false, false);
    let (mut line_breaks, mut special_characters) = (false, false);
    let (mut leading_space, mut leading_break) = (false, false);
    let (mut trailing_space, mut trailing_break) = (false, false);
    let (mut break_space, mut space_break) = (false, false);
    let head: String = s.iter().take(3).collect();
    if head == "---" || head == "..." {
        block_indicators = true;
        flow_indicators = true;
    }
    let mut preceded_by_whitespace = true;
    let mut followed_by_whitespace = s.len() == 1 || ws(s[1]);
    let (mut previous_space, mut previous_break) = (false, false);
    let last = s.len() - 1;
    for (index, &ch) in s.iter().enumerate() {
        if index == 0 {
            if "#,[]{}&*!|>'\"%@`".contains(ch) {
                flow_indicators = true;
                block_indicators = true;
            }
            if ch == '?' || ch == ':' {
                flow_indicators = true;
                block_indicators |= followed_by_whitespace;
            }
            if ch == '-' && followed_by_whitespace {
                flow_indicators = true;
                block_indicators = true;
            }
        } else {
            if ",?[]{}".contains(ch) {
                flow_indicators = true;
            }
            if ch == ':' {
                flow_indicators = true;
                block_indicators |= followed_by_whitespace;
            }
            if ch == '#' && preceded_by_whitespace {
                flow_indicators = true;
                block_indicators = true;
            }
        }
        if is_break(ch) {
            line_breaks = true;
        }
        // `allow_unicode=True`: printable non-ASCII is written as it is.
        special_characters |= !is_printable(ch);
        if ch == ' ' {
            leading_space |= index == 0;
            trailing_space |= index == last;
            break_space |= previous_break;
            previous_space = true;
            previous_break = false;
        } else if is_break(ch) {
            leading_break |= index == 0;
            trailing_break |= index == last;
            space_break |= previous_space;
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }
        preceded_by_whitespace = ws(ch);
        followed_by_whitespace = index + 2 >= s.len() || ws(s[index + 2]);
    }
    let mut a = Analysis {
        empty: false,
        multiline: line_breaks,
        allow_flow_plain: true,
        allow_block_plain: true,
        allow_single_quoted: true,
    };
    if leading_space || leading_break || trailing_space || trailing_break {
        a.allow_flow_plain = false;
        a.allow_block_plain = false;
    }
    if break_space || space_break || special_characters {
        a.allow_flow_plain = false;
        a.allow_block_plain = false;
        a.allow_single_quoted = false;
    }
    if line_breaks {
        a.allow_flow_plain = false;
        a.allow_block_plain = false;
    }
    if flow_indicators {
        a.allow_flow_plain = false;
    }
    if block_indicators {
        a.allow_block_plain = false;
    }
    a
}
