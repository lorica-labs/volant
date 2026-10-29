// SPDX-License-Identifier: GPL-3.0-or-later
//! What a YAML text may ask of the loader: how deep its collections nest, how many documents it
//! holds, and how much its anchors and aliases copy.
//!
//! The parser's own `load` calls itself once for each level, and so do the conversions that
//! follow it, so a text with a few thousand levels overflows the stack of the thread that reads
//! it. The loader also copies a node at each anchor and at each alias, so a text of a few hundred
//! bytes can stand for gigabytes. [`feed`] reads the parser's events one at a time and refuses a
//! text before the loader builds what it asks for.

use std::collections::HashMap;

use saphyr_parser::{Event, Marker, Parser, ScalarStyle, ScanError, SpannedEventReceiver};

/// What PyYAML says of a stream that holds a second document.
pub(super) const SECOND_DOCUMENT: &str =
    "expected a single document in the stream but found another document";

/// The deepest nesting of collections a document may have: the level ansible-core 2.19.12 loads
/// a file down to. It stops a file at 490 with `maximum recursion depth exceeded`, whether the
/// levels are block or flow collections.
///
/// Every walk over a document keeps its own stack (see `walk`), so a level costs about 160 bytes
/// of thread stack, 240 with a tag on it, in a debug build, where a recursive walk cost 3.6 KB
/// and 5.7 KB. The deepest document converts on 80 to 120 KB. The scanner stops a flow
/// collection at 255 levels (its counter is a `u8`), so deeper flow collections that
/// ansible-core loads are refused before this check.
pub(super) const MAX_DEPTH: usize = 489;

/// The most nodes anchors and aliases may copy in one text. The loader keeps a copy of every
/// anchored node and makes another at every alias. A text that holds no alias, or that keeps to
/// a few shared mappings, is far below this.
pub(super) const MAX_COPIED_NODES: usize = 1_000_000;

/// The most bytes of scalar text anchors and aliases may copy in one text. A scalar is one node
/// whatever its length, and each copy of it is made again by the conversions that follow, so the
/// text is counted too: 64 MiB of copies take about 200 MB of memory, as much as the node limit
/// does. A certificate of 2 KB shared by 1000 hosts is 2 MB.
pub(super) const MAX_COPIED_BYTES: usize = 64 * 1024 * 1024;

/// How much a node holds: the nodes in it, itself included, the bytes of its scalars, and how many
/// collections deep its longest path goes (`0` for a scalar).
#[derive(Clone, Copy)]
struct Size {
    nodes: usize,
    bytes: usize,
    height: usize,
}

/// A collection that is still open: the node it will be, once it closes.
struct Open {
    /// Its anchor, `0` when it has none.
    anchor: usize,
    size: Size,
}

/// What the events seen so far say about the size of the text.
#[derive(Default)]
struct Guard {
    documents: usize,
    open: Vec<Open>,
    /// The size of each anchored node, or `None` while it is open.
    anchored: HashMap<usize, Option<Size>>,
    copied_nodes: usize,
    copied_bytes: usize,
}

impl Guard {
    /// Takes the next event into account, or says why the text is refused.
    fn admit(&mut self, event: &Event<'_>, at: Marker) -> Result<(), ScanError> {
        match event {
            Event::DocumentStart(_) => {
                // PyYAML, and so every loader of ansible-core, stops at the second document.
                // Refusing it here keeps a stream from adding up to more than one text may.
                self.documents += 1;
                if self.documents > 1 {
                    return Err(ScanError::new_str(at, SECOND_DOCUMENT));
                }
            }
            Event::SequenceStart(anchor, _) | Event::MappingStart(anchor, _) => {
                if self.open.len() >= MAX_DEPTH {
                    return Err(too_deep(at));
                }
                if *anchor > 0 {
                    self.anchored.insert(*anchor, None);
                }
                self.open.push(Open {
                    anchor: *anchor,
                    size: Size {
                        nodes: 1,
                        bytes: 0,
                        height: 1,
                    },
                });
            }
            Event::SequenceEnd | Event::MappingEnd => {
                if let Some(done) = self.open.pop() {
                    if done.anchor > 0 {
                        self.anchored.insert(done.anchor, Some(done.size));
                        self.copy(done.size, at)?;
                    }
                    self.child(done.size);
                }
            }
            Event::Scalar(text, _, anchor, _) => {
                let size = Size {
                    nodes: 1,
                    bytes: text.len(),
                    height: 0,
                };
                if *anchor > 0 {
                    self.anchored.insert(*anchor, Some(size));
                    self.copy(size, at)?;
                }
                self.child(size);
            }
            Event::Alias(anchor) => {
                // An alias of a collection that is still open names a node that does not exist
                // yet: the loader gives it a placeholder, which counts as one node.
                let size = match self.anchored.get(anchor) {
                    Some(Some(size)) => *size,
                    Some(None) => Size {
                        nodes: 1,
                        bytes: 0,
                        height: 0,
                    },
                    None => {
                        return Err(ScanError::new_str(
                            at,
                            "while parsing node, found unknown anchor",
                        ));
                    }
                };
                if self.open.len() + size.height > MAX_DEPTH {
                    return Err(too_deep(at));
                }
                self.copy(size, at)?;
                self.child(size);
            }
            Event::Nothing | Event::StreamStart | Event::StreamEnd | Event::DocumentEnd => {}
        }
        Ok(())
    }

    /// Adds a finished node to the collection that holds it.
    fn child(&mut self, node: Size) {
        if let Some(parent) = self.open.last_mut() {
            parent.size.nodes = parent.size.nodes.saturating_add(node.nodes);
            parent.size.bytes = parent.size.bytes.saturating_add(node.bytes);
            parent.size.height = parent.size.height.max(node.height + 1);
        }
    }

    fn copy(&mut self, node: Size, at: Marker) -> Result<(), ScanError> {
        self.copied_nodes = self.copied_nodes.saturating_add(node.nodes);
        self.copied_bytes = self.copied_bytes.saturating_add(node.bytes);
        if self.copied_nodes > MAX_COPIED_NODES {
            return Err(ScanError::new(
                at,
                format!(
                    "the anchors and aliases of the text copy more than {MAX_COPIED_NODES} nodes"
                ),
            ));
        }
        if self.copied_bytes > MAX_COPIED_BYTES {
            return Err(ScanError::new(
                at,
                format!(
                    "the anchors and aliases of the text copy more than {MAX_COPIED_BYTES} bytes"
                ),
            ));
        }
        Ok(())
    }
}

/// Whether `info` is one of this engine's own limits rather than a syntax error: the reference
/// stops those elsewhere and in other words, so they are not worded as its YAML errors.
fn own_limit(info: &str) -> bool {
    info.starts_with("maximum recursion depth exceeded")
        || info.starts_with("the anchors and aliases of the text copy")
}

/// A file's syntax error, carrying the reference's `[ERROR]` block for it on top of the parser's
/// own: `YAML parsing failed:`, the sentence, and the place. The parser's error stays the root,
/// for [`super::is_second_document`]. Text that is no file keeps the parser's error alone.
pub(super) fn scan_located(err: anyhow::Error, text: &str, source: &str) -> anyhow::Error {
    let Some((message, line, col)) = syntax_error(&err, text) else {
        return err;
    };
    let Ok(path) = std::fs::canonicalize(source) else {
        return err;
    };
    let located = crate::playbook::Located {
        message,
        origin: crate::playbook::Origin {
            path: path.display().to_string().into(),
            line,
            col,
        },
    };
    err.context(located)
}

/// The reference's sentence for the syntax error `err` holds, `YAML parsing failed:` and all, with
/// its line and column counted from 1.
pub(super) fn syntax_error(err: &anyhow::Error, text: &str) -> Option<(String, usize, usize)> {
    let scan = err
        .root_cause()
        .downcast_ref::<ScanError>()
        .filter(|scan| !own_limit(scan.info()))?;
    let mut info = scan.info().to_string();
    // libyaml's word for a second document that does not open with `---`, which only an
    // explicit `...` end lets through.
    let opened = text
        .get(scan.marker().index()..)
        .is_some_and(|rest| rest.starts_with("---") || rest.starts_with('%'));
    if info == SECOND_DOCUMENT && !opened {
        info = "did not find expected <document start>".to_string();
    }
    // libyaml joins what it was parsing and what went wrong with a space, saphyr with a comma.
    if info.starts_with("while ")
        && let Some(at) = info.find(", ")
    {
        info.replace_range(at..at + 2, " ");
    }
    if let Some(first) = info.get(..1) {
        info.replace_range(..1, &first.to_uppercase());
    }
    let at = scan.marker();
    Some((
        format!("YAML parsing failed: {info}."),
        at.line(),
        at.col() + 1,
    ))
}

fn too_deep(at: Marker) -> ScanError {
    ScanError::new(
        at,
        format!("maximum recursion depth exceeded: collections nest more than {MAX_DEPTH} levels"),
    )
}

/// Parses `text` into `receiver`, as `Parser::load` does for a stream of documents, and refuses
/// a text that nests too deep, holds a second document, or copies too much before the receiver
/// sees the event that goes over the limit.
pub(super) fn feed<'a, R: SpannedEventReceiver<'a>>(
    text: &'a str,
    receiver: &mut R,
) -> Result<(), ScanError> {
    let mut guard = Guard::default();
    let mut keys = Keys::default();
    for event in Parser::new_from_str(text) {
        let (event, span) = event?;
        guard.admit(&event, span.start)?;
        keys.see(&event, span.start);
        receiver.on_event(event, span);
    }
    DUPLICATES.with(|d| d.borrow_mut().extend(keys.duplicates));
    Ok(())
}

/// A key written twice in one mapping: the key, and the line and column of the second one, both
/// counted from 1.
pub(super) type Duplicate = (String, usize, usize);

std::thread_local! {
    /// The duplicate keys of the texts this thread fed, taken by [`take_duplicates`].
    static DUPLICATES: std::cell::RefCell<Vec<Duplicate>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The duplicate keys [`feed`] found since the last call.
pub(super) fn take_duplicates() -> Vec<Duplicate> {
    DUPLICATES.with(|d| std::mem::take(&mut *d.borrow_mut()))
}

/// A mapping key as PyYAML's constructor compares it, and as its warning names it (Python's
/// `repr`). A plain scalar that reads as a number or a boolean is that number, so `1`, `1.0` and
/// `true` are one key, measured on ansible-core 2.19.12: `Found duplicate mapping key True.`
fn key_identity(text: &str, plain: bool) -> (String, String) {
    let resolved = if plain {
        super::scalar::plain(std::borrow::Cow::Borrowed(text)).ok()
    } else {
        None
    };
    match resolved.as_ref().and_then(super::walk::key_number) {
        Some(number) => (
            format!("#{number}"),
            super::walk::key_repr(resolved.as_ref()),
        ),
        None if matches!(resolved, Some(saphyr::Yaml::Value(saphyr::Scalar::Null))) => {
            ("~".to_string(), "None".to_string())
        }
        None => (format!("'{text}"), super::scalar::py_repr(text)),
    }
}

/// The scalar keys of every open mapping, to find one written twice as PyYAML's constructor
/// does: the same text as a key of the same mapping, a merge key `<<` aside.
#[derive(Default)]
struct Keys {
    /// One entry per open collection: a mapping's keys and how many of its nodes are done.
    open: Vec<Option<(std::collections::HashSet<String>, usize)>>,
    duplicates: Vec<Duplicate>,
}

impl Keys {
    fn see(&mut self, event: &Event<'_>, at: Marker) {
        match event {
            Event::Scalar(text, style, _, tag) => {
                if let Some(Some((keys, done))) = self.open.last_mut() {
                    let plain = *style == ScalarStyle::Plain && tag.is_none();
                    let (same, shown) = key_identity(text, plain);
                    if *done % 2 == 0 && text.as_ref() != "<<" && !keys.insert(same) {
                        self.duplicates.push((shown, at.line(), at.col() + 1));
                    }
                    *done += 1;
                }
            }
            Event::Alias(_) => self.done(),
            Event::MappingStart(..) => self.open.push(Some(Default::default())),
            Event::SequenceStart(..) => self.open.push(None),
            Event::MappingEnd | Event::SequenceEnd => {
                self.open.pop();
                self.done();
            }
            _ => {}
        }
    }

    /// A node of the innermost collection is complete.
    fn done(&mut self) {
        if let Some(Some((_, done))) = self.open.last_mut() {
            *done += 1;
        }
    }
}
