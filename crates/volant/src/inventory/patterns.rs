// SPDX-License-Identifier: GPL-3.0-or-later
//! Host patterns: splitting terms, `fnmatch`-style globs and `[N:M]` subscripts.

use std::sync::LazyLock;

use regex::Regex;

use super::Host;

/// The terms of a host pattern, as `split_host_pattern` of ansible-core 2.19.12 cuts them. A
/// pattern with a comma is that comma-separated list, and a `:` in one of its terms stays. A
/// pattern with none is one term when it is a valid address or host name with a port or a range
/// (`::1`, `fe80::1`, `[::1]:22`, `web[0:1]`), and a `:`-separated list otherwise: a bracketed
/// part holds its colons (`web[0:1]:db` is `web[0:1]` and `db`), and a lone `[` is dropped.
pub(super) fn split_terms(pattern: &str) -> Vec<&str> {
    static COLON_LIST: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:[^\s:\[\]]|\[[^\]]*\])+").expect("a valid pattern"));
    let terms: Vec<&str> = if pattern.contains(',') {
        pattern.split(',').collect()
    } else if pattern.contains(|c: char| c.is_whitespace() || matches!(c, ':' | '[' | ']')) {
        if is_one_address(pattern) {
            vec![pattern]
        } else {
            COLON_LIST.find_iter(pattern).map(|m| m.as_str()).collect()
        }
    } else {
        // No colon, bracket or blank: one term whether or not it is an address.
        vec![pattern]
    };
    terms
        .into_iter()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect()
}

/// Whether `parse_address` takes `pattern` whole. The address expressions are costly to build,
/// so a pattern they cannot match is turned away first: with a colon outside the brackets it is
/// an IPv6 address, which has hexadecimal digits and `.:[]` only, or a name with a port, which
/// ends in `:` and digits.
pub(super) fn is_one_address(pattern: &str) -> bool {
    let mut depth = 0usize;
    let mut bare_colon = false;
    for c in pattern.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => bare_colon = true,
            _ => {}
        }
    }
    if bare_colon {
        let hex = pattern
            .chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.' | '[' | ']'));
        let port = pattern
            .rsplit_once(':')
            .is_some_and(|(_, tail)| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()));
        if !hex && !port {
            return false;
        }
    }
    super::ranges::parse_address(pattern, true).is_ok()
}

/// `fnmatch` as Python does it for host patterns: `*` any run, `?` one character, `[abc]` and
/// `[a-z]` sets, `[!abc]` negated sets. No escaping, like Ansible. Ansible only reaches this for
/// bracket content that isn't a bare `[N]`/`[N:M]` subscript; see `split_subscript`.
///
/// Iterative, not recursive: a left-to-right scan that remembers only the most recent `*` and
/// where it last resumed matching from (the standard glob-matching algorithm), so the cost is
/// `O(pattern * text)`. The previous recursive form tried every split point of every `*` and
/// revisited the same `(pattern, text)` position through more than one path, which made a pattern
/// with many stars (`*a*a*a*a*a*a*a*a*a*a*b` against a 40-character miss) cost time exponential in
/// the number of stars — a hang, not an error, on an ordinary `--limit` pattern.
///
/// Measured against CPython's own `fnmatch.fnmatchcase` (`ansible-core` 2.19.12's bundled
/// 3.13.15), contrary to this function's own earlier doc comment and unit test: an unclosed `[`,
/// an empty `[]`, and a bare `[!]` are not "matches nothing" — they are literal text, because
/// CPython's `fnmatch.translate` falls back to a literal `[` (and re-scans the rest of the pattern
/// normally) whenever no closing `]` can be found. `parse_class` below reproduces that closing-`]`
/// search exactly, including its one subtlety: a `]` immediately after `[` or after `[!` counts as
/// a literal first set member, not the terminator, which is also why an empty `[]` and a bare
/// `[!]` fail to find a real close and fall back to literal text.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // The pattern index of the most recent unconsumed `*`, and the text index it last resumed
    // matching from — `None` until the first `*` is seen, at which point backtracking on a
    // mismatch means "let that `*` eat one more character" instead of failing outright.
    let mut backtrack: Option<(usize, usize)> = None;
    while ti < t.len() {
        if p.get(pi) == Some(&'*') {
            backtrack = Some((pi, ti));
            pi += 1;
            continue;
        }
        if pi < p.len() {
            let (hit, consumed) = atom_matches(&p, pi, t[ti]);
            if hit {
                pi += consumed;
                ti += 1;
                continue;
            }
        }
        match backtrack {
            Some((star, resumed_from)) => {
                ti = resumed_from + 1;
                pi = star + 1;
                backtrack = Some((star, ti));
            }
            None => return false,
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

/// Whether the pattern atom at `p[i]` (never `*`, the caller handles that separately) matches
/// `c`, and how many pattern characters that atom occupies: `1` for a literal or `?`, or the
/// whole bracket for a `[...]` class that actually closes, falling back to a single literal `[`
/// otherwise.
fn atom_matches(p: &[char], i: usize, c: char) -> (bool, usize) {
    match p[i] {
        '?' => (true, 1),
        '[' => match parse_class(p, i + 1) {
            Some((negate, set, end)) => (class_hit(set, c) != negate, end - i),
            None => (c == '[', 1),
        },
        literal => (c == literal, 1),
    }
}

/// A `[...]` class body starting right after the `[`, following CPython's `fnmatch.translate`:
/// a leading `!` negates, and the `]` immediately after that (or after a plain `[`) is a literal
/// first set member rather than the closing delimiter — the rule that makes an empty `[]` and a
/// bare `[!]` never find a close and fall back to literal text. Returns the negation flag, the
/// raw set body, and the pattern index right after the closing `]`; `None` when no `]` closes it.
fn parse_class(p: &[char], start: usize) -> Option<(bool, &[char], usize)> {
    let (negate, mut j) = match p.get(start) {
        Some('!') => (true, start + 1),
        _ => (false, start),
    };
    let body_start = j;
    if p.get(j) == Some(&']') {
        j += 1;
    }
    while j < p.len() && p[j] != ']' {
        j += 1;
    }
    if j >= p.len() {
        return None;
    }
    Some((negate, &p[body_start..j], j + 1))
}

/// Ceiling, measured against CPython's `fnmatch.fnmatchcase` over 400,000 random cases: a
/// reversed range such as `c-!` never matches anything here, while CPython's `fnmatch.translate`
/// merges it into no constraint at all, so `[?-*c-!!]?*` matches `b[b` there but not here.
fn class_hit(set: &[char], c: char) -> bool {
    let mut hit = false;
    let mut i = 0;
    while i < set.len() {
        if i + 2 < set.len() && set[i + 1] == '-' {
            hit |= set[i] <= c && c <= set[i + 2];
            i += 3;
        } else {
            hit |= set[i] == c;
            i += 1;
        }
    }
    hit
}

/// A single index (`[N]`, `N` may be negative) or an inclusive range (`[N:M]` or the deprecated
/// `[N-M]`, `N` and `M` plain digits, `M` optional meaning "to the end"). Measured against
/// `ansible-core`'s `PATTERN_WITH_SUBSCRIPT` regex: the range's own bounds are never negative in
/// text, only a lone index may be; anything else with a `[` (letters, a bare `:` with no leading
/// digit, `~` regexes) is not a subscript and falls through to `glob_match` instead, exactly as
/// the reference falls through to fnmatch for the same inputs.
pub(super) enum Subscript {
    Index(i64),
    Range(i64, Option<i64>),
}

pub(super) fn split_subscript(term: &str) -> Option<(&str, Subscript)> {
    if !term.ends_with(']') {
        return None;
    }
    let open = term.rfind('[')?;
    if open == 0 {
        return None;
    }
    let base = &term[..open];
    let inner = &term[open + 1..term.len() - 1];
    let is_signed_digits = |s: &str| {
        let s = s.strip_prefix('-').unwrap_or(s);
        !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
    };
    if is_signed_digits(inner) {
        return Some((base, Subscript::Index(saturating(inner))));
    }
    let sep = inner.find([':', '-'])?;
    let (start, end) = (&inner[..sep], &inner[sep + 1..]);
    let is_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if !is_digits(start) || (!end.is_empty() && !is_digits(end)) {
        return None;
    }
    let end = if end.is_empty() {
        None
    } else {
        Some(saturating(end))
    };
    Some((base, Subscript::Range(saturating(start), end)))
}

/// The integer `digits` spell, with an optional leading `-`. Python's integers have no end, so a
/// number past this one's still orders against a host count as it would there: it is clamped to
/// the largest or the smallest value, which no inventory is that long to tell from the real one.
fn saturating(digits: &str) -> i64 {
    digits.parse().unwrap_or(if digits.starts_with('-') {
        i64::MIN
    } else {
        i64::MAX
    })
}

/// Ansible's `_apply_subscript`: a Python-style negative index wraps from the end; a range's
/// missing end means "to the last host" and, unlike a Rust or Python range, its given end is
/// inclusive. Anything out of bounds is empty, not an error — the reference does not fail a
/// playbook run over an over-large subscript, it just selects nothing.
pub(super) fn apply_subscript(names: &[String], sub: Subscript) -> Vec<String> {
    let len = names.len() as i64;
    let wrap = |i: i64| if i < 0 { i + len } else { i };
    match sub {
        Subscript::Index(i) => {
            let i = wrap(i);
            if (0..len).contains(&i) {
                vec![names[i as usize].clone()]
            } else {
                Vec::new()
            }
        }
        // The reference reads an end of `0` as no end at all and takes the one host `start` names,
        // so `[1:0]` is `[1]`.
        Subscript::Range(start, Some(0)) => apply_subscript(names, Subscript::Index(start)),
        Subscript::Range(start, end) => {
            let start = start.clamp(0, len);
            let stop = end.unwrap_or(len - 1).saturating_add(1).clamp(0, len);
            if start >= stop {
                Vec::new()
            } else {
                names[start as usize..stop as usize].to_vec()
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct Resolution {
    pub hosts: Vec<Host>,
    pub unmatched: Vec<String>,
    pub warnings: Vec<String>,
    /// The first `~regex` term that does not compile: the reference stops on it with
    /// `Invalid host list pattern`, so the caller refuses the run.
    pub invalid: Option<String>,
    /// The first spelling of the implicit controller host a pattern names: it stands for the host.
    pub(super) implicit: Option<String>,
}
