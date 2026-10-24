// SPDX-License-Identifier: GPL-3.0-or-later
//! Literal block scalars, which only the callbacks' dumper writes.

use super::super::scalar::is_break;
use super::Emitter;

impl Emitter {
    /// libyaml's `yaml_emitter_write_literal_scalar` and its block scalar hints: `|`, the
    /// indentation when the text starts with a space or a break, the chomping, then each line at
    /// the indentation and every break as it is.
    pub(super) fn write_literal(&mut self, text: &[char]) {
        self.write_indicator("|", true, false, false);
        if text.first().is_some_and(|c| *c == ' ' || is_break(*c)) {
            let hint = self.best_indent.to_string();
            self.write_indicator(&hint, false, false, false);
        }
        // Strip when the text does not end with a break, keep when it ends with two or is one.
        let chomp = match text {
            [.., last] if is_break(*last) => {
                (text.len() == 1 || is_break(text[text.len() - 2])).then_some("+")
            }
            _ => Some("-"),
        };
        if let Some(chomp) = chomp {
            self.write_indicator(chomp, false, false, false);
        }
        self.write_line_break('\n');
        let mut breaks = true;
        for &ch in text {
            if is_break(ch) {
                self.write_line_break(ch);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                    breaks = false;
                }
                self.write(&[ch]);
                self.indention = false;
            }
        }
    }
}

/// `_munge_data_for_lossy_yaml`: trailing whitespace off, tabs expanded, the characters libyaml
/// calls special dropped, and the spaces before a line break with them.
pub(super) fn lossy(text: &str) -> String {
    let trimmed = text.trim_end_matches(char::is_whitespace);
    // `str.expandtabs()`: to the next multiple of 8, the column counted from the last `\n` or
    // `\r`.
    let mut expanded = String::new();
    let mut column = 0;
    for ch in trimmed.chars() {
        match ch {
            '\t' => {
                let n = 8 - column % 8;
                expanded.extend(std::iter::repeat_n(' ', n));
                column += n;
            }
            '\n' | '\r' => {
                expanded.push(ch);
                column = 0;
            }
            _ => {
                expanded.push(ch);
                column += 1;
            }
        }
    }
    let kept = expanded.chars().filter(|&c| {
        let n = u32::from(c);
        (n == 10
            || (32..=126).contains(&n)
            || n == 133
            || (160..=55295).contains(&n)
            || (57344..=65533).contains(&n)
            || (65536..1_114_111).contains(&n))
            && n != 65279
    });
    // ` +([\n\x85\u2028\u2029])` replaced by the break alone.
    let mut out = String::new();
    let mut spaces = 0;
    for ch in kept {
        if ch == ' ' {
            spaces += 1;
            continue;
        }
        if !matches!(ch, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            out.extend(std::iter::repeat_n(' ', spaces));
        }
        spaces = 0;
        out.push(ch);
    }
    out.extend(std::iter::repeat_n(' ', spaces));
    out
}
