// SPDX-License-Identifier: GPL-3.0-or-later
//! Python's `fnmatch.fnmatch`, which the `filter` argument of `setup` is matched with.

/// One element of a pattern, as `fnmatch.translate` reads it.
enum Glob {
    Star,
    Any,
    Char(char),
    /// `[...]`: negated, and its members as ranges (a single character is a range of one).
    Set(bool, Vec<(char, char)>),
}

/// Python's `fnmatch.fnmatch` on a POSIX host: case-sensitive, over the whole name, with `*`,
/// `?` and `[...]` read as `fnmatch.translate` reads them.
pub(super) fn fnmatch(name: &str, pattern: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let mut globs = Vec::new();
    let mut i = 0;
    while i < pat.len() {
        let c = pat[i];
        i += 1;
        match c {
            '*' => globs.push(Glob::Star),
            '?' => globs.push(Glob::Any),
            '[' => {
                let mut j = i;
                if pat.get(j) == Some(&'!') {
                    j += 1;
                }
                if pat.get(j) == Some(&']') {
                    j += 1;
                }
                while j < pat.len() && pat[j] != ']' {
                    j += 1;
                }
                // An unclosed `[` is a character like any other.
                if j >= pat.len() {
                    globs.push(Glob::Char('['));
                    continue;
                }
                globs.push(glob_set(&pat[i..j]));
                i = j + 1;
            }
            c => globs.push(Glob::Char(c)),
        }
    }
    let one = |glob: &Glob, c: char| match glob {
        Glob::Star => false,
        Glob::Any => true,
        Glob::Char(g) => *g == c,
        Glob::Set(negated, members) => {
            members.iter().any(|(lo, hi)| (*lo..=*hi).contains(&c)) != *negated
        }
    };
    let text: Vec<char> = name.chars().collect();
    let (mut p, mut s) = (0, 0);
    // The last `*` seen, and the position in the name it is currently taken to end at.
    let mut star: Option<(usize, usize)> = None;
    while s < text.len() {
        if p < globs.len() && matches!(globs[p], Glob::Star) {
            star = Some((p, s));
            p += 1;
        } else if p < globs.len() && one(&globs[p], text[s]) {
            p += 1;
            s += 1;
        } else if let Some((sp, ss)) = star {
            star = Some((sp, ss + 1));
            p = sp + 1;
            s = ss + 1;
        } else {
            return false;
        }
    }
    globs[p..].iter().all(|g| matches!(g, Glob::Star))
}

/// What sits between the brackets, read as `fnmatch.translate` reads it: a leading `!`
/// negates; a `-` between two characters is a range, but not the first one (nor the one right
/// after a range's end), and a trailing one is literal; a range whose ends are reversed is
/// dropped with both its ends. A set left empty matches nothing, and negated matches anything.
fn glob_set(stuff: &[char]) -> Glob {
    let negated = stuff.first() == Some(&'!');
    let mut chunks: Vec<Vec<char>> = Vec::new();
    let mut from = 0;
    let mut k = if negated { 2 } else { 1 };
    while let Some(at) = stuff
        .get(k..)
        .and_then(|rest| rest.iter().position(|c| *c == '-'))
    {
        chunks.push(stuff[from..k + at].to_vec());
        from = k + at + 1;
        k += at + 3;
    }
    let last = stuff.get(from..).unwrap_or_default().to_vec();
    match chunks.last_mut() {
        Some(previous) if last.is_empty() => previous.push('-'),
        _ => chunks.push(last),
    }
    for k in (1..chunks.len()).rev() {
        let (Some(&end), Some(&start)) = (chunks[k - 1].last(), chunks[k].first()) else {
            continue;
        };
        if end > start {
            chunks[k - 1].pop();
            let tail = chunks.remove(k);
            chunks[k - 1].extend_from_slice(&tail[1..]);
        }
    }
    if negated && let Some(first) = chunks.first_mut() {
        first.remove(0);
    }
    let mut members: Vec<(char, char)> = chunks.iter().flatten().map(|c| (*c, *c)).collect();
    for pair in chunks.windows(2) {
        if let (Some(&lo), Some(&hi)) = (pair[0].last(), pair[1].first()) {
            members.push((lo, hi));
        }
    }
    Glob::Set(negated, members)
}
