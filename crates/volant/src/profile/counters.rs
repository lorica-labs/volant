// SPDX-License-Identifier: GPL-3.0-or-later
//! The run's counters, in statics: the template engine and the bridge, which bump them, never
//! see the run's [`Profile`](super::Profile).

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash as _, Hasher as _};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Something the run does often enough to be worth counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    /// A `when`, `changed_when`, `failed_when`, `until`, `break_when` or `assert`/`fail` `that`
    /// condition evaluated.
    RenderWhen,
    /// A task's arguments rendered for one item.
    RenderArgs,
    /// A loop's list, or a `with_` lookup's terms, rendered.
    RenderLoop,
    /// A host's variables resolved against each other.
    RenderVars,
    /// One host's entry of `hostvars` converted for a render.
    RenderHostvars,
    /// An expression compiled.
    Compile,
    /// What a template reads, found in the cache.
    ReadsHit,
    /// What a template reads, worked out.
    ReadsMiss,
    /// A host's variable layers built.
    LayersBuilt,
    /// The entries those layers copied.
    LayersEntries,
    /// A plugin run through the Python helper.
    BridgeCalls,
    /// The time those runs took.
    BridgeNanos,
    /// Frame payload bytes written to the agents.
    WireOutBytes,
    /// Frame payload bytes read from the agents.
    WireInBytes,
}

/// The counters' names, in declaration order.
const NAMES: [&str; 14] = [
    "render_when",
    "render_args",
    "render_loop",
    "render_vars",
    "render_hostvars",
    "compile",
    "reads_hit",
    "reads_miss",
    "layers_built",
    "layers_entries",
    "bridge_calls",
    "bridge_nanos",
    "wire_out_bytes",
    "wire_in_bytes",
];

static COUNTS: [AtomicU64; NAMES.len()] = [const { AtomicU64::new(0) }; NAMES.len()];

/// Whether the per-plugin table and the repeated renders are kept.
static TRACKING: AtomicBool = AtomicBool::new(false);

/// Calls and nanoseconds per bridge plugin, while tracking.
static BRIDGE: Mutex<BTreeMap<String, (u64, u64)>> = Mutex::new(BTreeMap::new());

static REPEATS: Mutex<Option<Repeats>> = Mutex::new(None);

/// The prepares of the run so far, one per item: see [`next_prepare`].
static PREPARES: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Repeats {
    /// How often each (prepare, host, text) was rendered, by hash.
    seen: HashMap<u64, u32>,
    /// Texts rendered again inside one item's prepare.
    again: Tally,
    /// Texts a loop item after the first renders without reading the loop's variables.
    invariant: Tally,
}

#[derive(Default)]
struct Tally {
    total: u64,
    texts: HashMap<String, u64>,
}

impl Tally {
    fn add(&mut self, text: &str) {
        self.total += 1;
        *self.texts.entry(text.to_string()).or_default() += 1;
    }

    fn top(&self, top: usize) -> (u64, Vec<(String, u64)>) {
        let mut texts: Vec<(String, u64)> = self
            .texts
            .iter()
            .map(|(text, n)| (text.clone(), *n))
            .collect();
        texts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        texts.truncate(top);
        (self.total, texts)
    }
}

fn with_repeats<T>(f: impl FnOnce(&mut Repeats) -> T) -> T {
    let mut guard = REPEATS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(Repeats::default))
}

/// Adds `by` to `counter`.
pub fn count(counter: Counter, by: u64) {
    COUNTS[counter as usize].fetch_add(by, Ordering::Relaxed);
}

/// Every counter by its snake_case name, in declaration order.
pub fn counters() -> Vec<(&'static str, u64)> {
    NAMES
        .iter()
        .zip(&COUNTS)
        .map(|(name, value)| (*name, value.load(Ordering::Relaxed)))
        .collect()
}

/// Starts a run's counting: every counter back to zero, and the per-plugin bridge table and the
/// repeated renders kept only when `on`. A run turns them on when it is asked for a profile, and
/// never pays for them otherwise.
pub fn track_repeats(on: bool) {
    for value in &COUNTS {
        value.store(0, Ordering::Relaxed);
    }
    BRIDGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    *REPEATS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    TRACKING.store(on, Ordering::Relaxed);
}

/// Whether [`track_repeats`] is on, for a caller that would walk a value to note its texts.
pub fn tracks_repeats() -> bool {
    TRACKING.load(Ordering::Relaxed)
}

/// A number for the prepare of one item, never handed out twice in a run: a loop item, a retry
/// and a second play each prepare anew, and none of them is a repeat of the other.
pub fn next_prepare() -> u64 {
    PREPARES.fetch_add(1, Ordering::Relaxed)
}

/// Notes that the item's `prepare` rendered `text` for `host`. Nothing unless tracking.
pub fn note_render(prepare: u64, host: &str, text: &str) {
    if !tracks_repeats() {
        return;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (prepare, host, text).hash(&mut hasher);
    let key = hasher.finish();
    with_repeats(|repeats| {
        let seen = repeats.seen.entry(key).or_default();
        *seen = seen.saturating_add(1);
        if *seen > 1 {
            repeats.again.add(text);
        }
    });
}

/// Notes that a loop item after the first rendered `text`, which reads none of the loop's
/// variables. Nothing unless tracking.
pub fn note_invariant(text: &str) {
    if tracks_repeats() {
        with_repeats(|repeats| repeats.invariant.add(text));
    }
}

/// The renders that repeated one already done inside the same item's prepare, and the `top`
/// texts repeated most, most first.
pub fn repeats(top: usize) -> (u64, Vec<(String, u64)>) {
    with_repeats(|repeats| repeats.again.top(top))
}

/// The loop-invariant renders [`note_invariant`] counted, as [`repeats`].
pub fn loop_invariant(top: usize) -> (u64, Vec<(String, u64)>) {
    with_repeats(|repeats| repeats.invariant.top(top))
}

/// Counts one bridge call to the plugin `name` that took `nanos`.
pub fn bridge_call(name: &str, nanos: u64) {
    count(Counter::BridgeCalls, 1);
    count(Counter::BridgeNanos, nanos);
    if !tracks_repeats() {
        return;
    }
    let mut table = BRIDGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((calls, total)) = table.get_mut(name) {
        *calls += 1;
        *total = total.saturating_add(nanos);
    } else {
        table.insert(name.to_string(), (1, nanos));
    }
}

pub(super) fn bridge_table() -> BTreeMap<String, (u64, u64)> {
    BRIDGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The repeated texts a profile shows.
pub(super) const TOP_REPEATS: usize = 20;

/// A text on one line of at most 80 characters.
pub(super) fn clipped(text: &str) -> String {
    text.escape_debug().take(80).collect()
}
