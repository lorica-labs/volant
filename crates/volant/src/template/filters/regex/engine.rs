// SPDX-License-Identifier: GPL-3.0-or-later
//! A compiled Python pattern, and the walk over its matches that `re.findall` and `re.sub` make.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use minijinja::Error;
use regex::{Captures, Regex, RegexBuilder};

use super::invalid;
use super::pattern::{Failure, translate};

pub(super) struct Compiled {
    strict: Regex,
    /// The pattern with Python's `$`, exact on a text whose only newline is a final one.
    loose: Option<Regex>,
    /// The capture groups after the whole match.
    pub(super) groups: usize,
    /// The named groups and their numbers.
    pub(super) names: Vec<(String, usize)>,
    prefers_empty: bool,
    pattern: String,
}

type Cached = Result<Rc<Compiled>, String>;

thread_local! {
    /// What `compile` did for a pattern and its two flags, so that a filter in a loop over many
    /// lines compiles its pattern once.
    static CACHE: RefCell<HashMap<String, [Option<Cached>; 4]>> = RefCell::new(HashMap::new());
}

/// Entries kept before the cache starts again.
const CACHE_LIMIT: usize = 256;

/// `re.compile(pattern, flags)` with `ignorecase` and `multiline` as `re.I` and `re.M`.
pub(super) fn compile(
    pattern: &str,
    ignorecase: bool,
    multiline: bool,
) -> Result<Rc<Compiled>, Error> {
    let slot = usize::from(ignorecase) * 2 + usize::from(multiline);
    let hit = CACHE.with(|cache| {
        cache
            .borrow()
            .get(pattern)
            .and_then(|slots| slots[slot].clone())
    });
    let result = hit.unwrap_or_else(|| {
        let built = build(pattern, ignorecase, multiline).map(Rc::new);
        CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= CACHE_LIMIT {
                cache.clear();
            }
            cache.entry(pattern.to_string()).or_default()[slot] = Some(built.clone());
        });
        built
    });
    result.map_err(invalid)
}

fn build(pattern: &str, ignorecase: bool, multiline: bool) -> Result<Compiled, String> {
    let translated = translate(pattern, multiline).map_err(|failure| match failure {
        // Python's own words: the filter's or test's failure names the plugin around them.
        Failure::Invalid(why) => why,
        Failure::Unsupported(what) => format!(
            "unsupported regular expression '{pattern}': {what} is not supported by the regex engine of Volant"
        ),
    })?;
    let make = |source: &str| {
        RegexBuilder::new(source)
            .case_insensitive(ignorecase)
            .multi_line(multiline)
            .build()
            .map_err(|e| {
                let text = e.to_string();
                let why = text
                    .lines()
                    .last()
                    .map_or(text.as_str(), |l| l.strip_prefix("error: ").unwrap_or(l));
                why.to_owned()
            })
    };
    let strict = make(&translated.strict)?;
    let loose = translated.loose.as_deref().map(make).transpose()?;
    let names = strict
        .capture_names()
        .enumerate()
        .filter_map(|(i, name)| name.map(|n| (n.to_string(), i)))
        .collect();
    Ok(Compiled {
        groups: strict.captures_len() - 1,
        names,
        prefers_empty: translated.prefers_empty,
        strict,
        loose,
        pattern: pattern.to_string(),
    })
}

/// One match: where it lies, and its groups when they were asked for.
pub(super) struct Hit<'t> {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) caps: Option<Captures<'t>>,
}

impl Compiled {
    /// The engine to run on `text`. Python's `$` matches before a final newline, which the
    /// `regex` crate cannot say; it can on a text with no other newline.
    fn engine(&self, text: &str) -> Result<&Regex, Error> {
        let Some(loose) = &self.loose else {
            return Ok(&self.strict);
        };
        let body = text.strip_suffix('\n').unwrap_or(text);
        if !body.contains('\n') {
            Ok(loose)
        } else if body.len() == text.len() {
            Ok(&self.strict)
        } else {
            Err(invalid(format!(
                "unsupported regular expression '{}': '$' before the final newline of a text with more lines is not supported by the regex engine of Volant",
                self.pattern
            )))
        }
    }

    /// Python's `re.search`, with groups when `with_caps`.
    pub(super) fn search<'t>(
        &self,
        text: &'t str,
        with_caps: bool,
    ) -> Result<Option<Hit<'t>>, Error> {
        let re = self.engine(text)?;
        Ok(Self::at(re, text, 0, with_caps))
    }

    fn at<'t>(re: &Regex, text: &'t str, from: usize, with_caps: bool) -> Option<Hit<'t>> {
        if with_caps {
            let caps = re.captures_at(text, from)?;
            let whole = caps.get(0)?;
            Some(Hit {
                start: whole.start(),
                end: whole.end(),
                caps: Some(caps),
            })
        } else {
            let m = re.find_at(text, from)?;
            Some(Hit {
                start: m.start(),
                end: m.end(),
                caps: None,
            })
        }
    }

    /// Every match, as Python 3.7 and later walks them: an empty match may touch the end of the
    /// one before, and after an empty match the next one must not be empty at the same place.
    /// `visit` answers whether to go on.
    pub(super) fn scan<'t>(
        &self,
        text: &'t str,
        with_caps: bool,
        mut visit: impl FnMut(Hit<'t>) -> bool,
    ) -> Result<(), Error> {
        let re = self.engine(text)?;
        let mut from = 0;
        let mut must_advance = false;
        while from <= text.len() {
            let Some(hit) = Self::at(re, text, from, with_caps) else {
                break;
            };
            if must_advance && hit.start == from && hit.end == from {
                // Python would look here for a longer match the `regex` crate ranks behind
                // the empty one. That only happens when the pattern prefers an empty match.
                if self.prefers_empty {
                    return Err(invalid(format!(
                        "unsupported regular expression '{}': a lazy repeat or an empty alternative can match the empty text here, and the regex engine of Volant cannot look for a longer match",
                        self.pattern
                    )));
                }
                let Some(next) = text[from..].chars().next() else {
                    break;
                };
                from += next.len_utf8();
                must_advance = false;
                continue;
            }
            from = hit.end;
            must_advance = hit.start == hit.end;
            if !visit(hit) {
                break;
            }
        }
        Ok(())
    }
}
