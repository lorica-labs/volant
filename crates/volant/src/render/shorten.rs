// SPDX-License-Identifier: GPL-3.0-or-later
//! `textwrap.shorten`, which the error block shortens a value no file holds with.

/// Python's `textwrap.shorten(text, width)`: whitespace collapsed, then the one line
/// `TextWrapper(max_lines=1)` fills, ` [...]` behind it when the whole does not fit. A line is
/// built from the chunks `TextWrapper` splits words into, so it may end after a hyphen, and a
/// word longer than `width` is cut (`break_long_words`, `break_on_hyphens`).
pub fn shorten(text: &str, width: usize) -> String {
    const PLACEHOLDER: &str = " [...]";
    let len = |s: &str| s.chars().count();
    let mut rest = std::collections::VecDeque::new();
    for (i, word) in text.split_whitespace().enumerate() {
        if i > 0 {
            rest.push_back(" ".to_string());
        }
        rest.extend(word_chunks(word));
    }
    let width = width.max(1);
    let mut line: Vec<String> = Vec::new();
    let mut used = 0;
    while let Some(next) = rest.front() {
        if used + len(next) > width {
            break;
        }
        used += len(next);
        line.extend(rest.pop_front());
    }
    // `_handle_long_word`: a chunk no line can hold is cut where the line ends, after its last
    // hyphen there if one leaves something before it.
    if let Some(next) = rest.front_mut()
        && len(next) > width
        && used < width
    {
        let chars: Vec<char> = next.chars().collect();
        let room = width - used;
        let end = chars[..room]
            .iter()
            .rposition(|c| *c == '-')
            .filter(|&h| h > 0 && chars[..h].iter().any(|c| *c != '-'))
            .map_or(room, |h| h + 1);
        line.push(chars[..end].iter().collect());
        *next = chars[end..].iter().collect();
        used += end;
    }
    if line.last().is_some_and(|c| c.trim().is_empty()) {
        used -= line.pop().map_or(0, |c| len(&c));
    }
    let done = rest.is_empty() || (rest.len() == 1 && rest[0].trim().is_empty());
    if done && used <= width {
        return line.concat();
    }
    while let Some(last) = line.last() {
        if !last.trim().is_empty() && used + PLACEHOLDER.len() <= width {
            line.push(PLACEHOLDER.to_string());
            return line.concat();
        }
        used -= len(last);
        line.pop();
    }
    PLACEHOLDER.trim_start().to_string()
}

/// One word as `TextWrapper.wordsep_re` splits it: after a hyphen between two letters
/// (`some-long-name` is `some-`, `long-`, `name`), and around a run of two dashes or more
/// between words (`a--b` is `a`, `--`, `b`).
fn word_chunks(word: &str) -> Vec<String> {
    let c: Vec<char> = word.chars().collect();
    // `[^\d\W]`, `\w` and `[\w!"'&.,?]`.
    let letter = |i: usize| c.get(i).is_some_and(|ch| ch.is_alphabetic() || *ch == '_');
    let word_char = |i: usize| {
        c.get(i)
            .is_some_and(|ch| ch.is_alphanumeric() || *ch == '_')
    };
    let punct = |i: usize| {
        c.get(i)
            .is_some_and(|ch| ch.is_alphanumeric() || "_!\"'&.,?".contains(*ch))
    };
    // A run of two dashes or more at `i` with a word character behind it: its length.
    let em_dash = |i: usize| {
        let n = c[i..].iter().take_while(|ch| **ch == '-').count();
        (n >= 2 && word_char(i + n)).then_some(n)
    };
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < c.len() {
        if start > 0
            && punct(start - 1)
            && let Some(n) = em_dash(start)
        {
            chunks.push(c[start..start + n].iter().collect());
            start += n;
            continue;
        }
        let mut end = start + 1;
        start = loop {
            if end == c.len() {
                chunks.push(c[start..end].iter().collect());
                break end;
            }
            let hyphenated = c[end] == '-'
                && ((end >= 2 && letter(end - 2) && letter(end - 1))
                    || (end >= 3 && letter(end - 3) && c[end - 2] == '-' && letter(end - 1)))
                && letter(end + 1)
                && (letter(end + 2) || (c.get(end + 2) == Some(&'-') && letter(end + 3)));
            if hyphenated {
                chunks.push(c[start..=end].iter().collect());
                break end + 1;
            }
            if punct(end - 1) && em_dash(end).is_some() {
                chunks.push(c[start..end].iter().collect());
                break end;
            }
            end += 1;
        };
    }
    chunks
}
