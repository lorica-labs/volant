// SPDX-License-Identifier: GPL-3.0-or-later
//! The flat step list a play compiles to: steps, kinds, origins and block spans.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::playbook::PlayTask;
use crate::roles::{RoleSearch, RoleVars};

use super::tags::TagSelection;
/// What one step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepKind {
    /// A module to run on the host.
    Task,
    /// `meta`: something asked of the engine rather than of the host. It shows a banner and
    /// reports nothing, which is why it is the one step kind that prints once per live host.
    Meta,
    /// A flush point: where the notified handlers run. `explicit` is set for the one a playbook
    /// wrote as `meta: flush_handlers`, which shows the same `TASK [meta]` banner every other
    /// `meta` shows; the three the compiler puts in itself show nothing, measured.
    ///
    /// It is the one step whose *successors* are not known when the play is compiled: the
    /// coordinator inserts the handler steps behind it once every host has reported it, so a
    /// driver that has reported it waits before reading the list again.
    Flush { explicit: bool },
    /// One handler of [`Compiled::handlers`], spliced in behind a flush point. It runs for a host
    /// that notified it and reports nothing for a host that did not.
    Handler(usize),
    /// `include_tasks` or `include_role`: a statement naming work that is read while the play
    /// runs, because what it names may depend on the host reaching it.
    ///
    /// Like a flush point it is a splice point - its successors are not known when the play is
    /// compiled - so the two travel together everywhere [`is_splice_point`] is asked.
    Include(IncludeKind),
}

/// Which of the two dynamic statements a [`StepKind::Include`] step came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncludeKind {
    Tasks,
    Role,
}

/// Whether the coordinator has to insert steps behind this one before any host may read the list
/// again.
///
/// The two kinds are one rule and not two: a driver that reports such a step waits for
/// `spliced_through` to reach it, whether it ran the step or stepped over it, and the coordinator
/// publishes a splice at every one of them - an empty one when there was nothing to insert,
/// because a driver waiting on it has no way to learn that on its own. Asked through this one
/// predicate so a caller cannot honour the flush half and forget the include half, which is how a
/// host comes to hold an index the splice has moved.
pub(crate) fn is_splice_point(kind: &StepKind) -> bool {
    matches!(kind, StepKind::Flush { .. } | StepKind::Include(_))
}

/// One handler, with everything a flush needs to decide whether to run it and what to call it.
#[derive(Debug, Clone)]
pub(crate) struct Handler {
    pub task: PlayTask,
    /// The extra names this handler answers to. Measured on ansible-core 2.19.12: `listen` works
    /// for a name no handler carries as its `name`, and two handlers listening to one name both
    /// run, in definition order.
    pub listen: Vec<String>,
    /// The role instance it came from, which is what puts `base : ` in front of its banner and
    /// what decides which variable layers it reads.
    pub role: Option<usize>,
}

/// Which of a block's three lists a step came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Body,
    Rescue,
    Always,
}

/// Where a step was written, which is what a statement inside it resolves its relative paths
/// against and what bounds how deep an include may nest.
///
/// Shared behind an [`Arc`] because every step of one file has the same one: a role of a few
/// hundred tasks pays one of these rather than one per step, and the per-batch clone of the whole
/// step list copies a pointer instead of two paths.
#[derive(Debug, Clone)]
pub(crate) struct Origin {
    /// The directory a relative path written in this step is read against: the playbook's, a
    /// role's `tasks/`, or the directory of the file an include brought in.
    pub file_dir: PathBuf,
    /// The root of the role this step was compiled inside, if any. `include_vars` looks in its
    /// `vars/` before anywhere else, measured.
    pub role_dir: Option<PathBuf>,
    /// How many includes deep this step was spliced in. The statement at depth 32 is refused
    /// rather than followed: the reference recurses until Python's stack is gone and reports its
    /// own crash, with no recap and nothing to read.
    pub depth: u32,
    /// For an include statement, the keywords the layers around it gave it - the play, the
    /// blocks it sits in, the role entries and imports above it - without the ones it carries
    /// itself. What it brings in is compiled under these: the reference's included blocks skip
    /// the statement and inherit from its parent (`Block._get_parent_attribute`, "defer to the
    /// grandparent"). `None` for every other step.
    pub inherited: Option<Arc<PlayTask>>,
}

impl Default for Origin {
    fn default() -> Self {
        Origin {
            file_dir: PathBuf::from("."),
            role_dir: None,
            depth: 0,
            inherited: None,
        }
    }
}

/// What an include statement hands down, with the provenance of each name beside the values.
///
/// The values are rendered on the driver, against the variables of the item that asked for the
/// include, and travel to the steps below as text. A name rendered from a managed host's output
/// is data, and the text it holds is whatever that host wrote: without this set travelling with
/// the values, the merged map of every step the statement brought in resolves it as if the
/// playbook had written it.
#[derive(Debug, Clone, Default)]
pub(crate) struct IncludeParams {
    pub values: Map<String, Value>,
    /// The names in `values` whose render read something that came from a managed host.
    pub untrusted: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Step {
    pub kind: StepKind,
    /// The task with every keyword inherited from the blocks above it already merged in, so
    /// nothing downstream has to remember it was ever inside anything.
    pub task: PlayTask,
    /// The innermost block this step belongs to, if any.
    pub block: Option<usize>,
    pub section: Section,
    /// The role instance this step came from, if any: an index into [`Compiled::roles`]. It
    /// decides two things, both measured - the `role : task` prefix the banner carries, and
    /// which variable layers the driver puts under the play's.
    pub role: Option<usize>,
    /// Where this step was written, for the statements inside it that read a file.
    pub origin: Arc<Origin>,
    /// The variables the include statement that brought this step in handed down, shared by every
    /// step of that one expansion.
    ///
    /// They are a layer of their own rather than task variables because of where they sit:
    /// measured on ansible-core 2.19.12, `include_role: {name: base}` with `vars: {p: x}` reads
    /// `p=x` in the role even when the host carries a `set_fact` of that name, while the same
    /// `vars:` written on a `roles:` entry loses to that fact. So they go in at the role-parameter
    /// layer, above the facts and under `-e`, and they accumulate: an include inside an include
    /// hands its parent's down with its own.
    pub include_params: Option<Arc<IncludeParams>>,
    /// The hosts this step runs for, when it was spliced in for some of them and not all.
    ///
    /// `None` is every host, which is what the compilation produces. A step an include brought in
    /// carries the hosts whose own include asked for it: measured on ansible-core 2.19.12, an
    /// include a `when` left out for one of two hosts inserts its tasks for the other alone, and
    /// the host outside the list shows no line under their banners. Every host still reports the
    /// step, because a barrier behind it opens on hosts that have passed it and not on hosts that
    /// had a reason to run it.
    pub hosts: Option<Arc<[String]>>,
}

/// Where one block's three sections landed in the flat list.
#[derive(Debug, Clone)]
pub(crate) struct BlockSpan {
    pub body: Range<usize>,
    pub rescue: Range<usize>,
    pub always: Range<usize>,
    pub parent: Option<usize>,
    /// Which section of `parent` this block sits in. Kept here rather than worked back out of
    /// the indices: reading it off the step before the one being left is the kind of arithmetic
    /// that quietly points one past a span.
    pub section: Section,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Compiled {
    pub steps: Vec<Step>,
    pub blocks: Vec<BlockSpan>,
    /// One entry per role instance the play runs, in the order they run. A step naming one
    /// reads its variables from here; a step naming none reads [`Compiled::exported`].
    pub roles: Vec<RoleVars>,
    /// What every role in the play lends to everything else in it.
    ///
    /// Measured on ansible-core 2.19.12: a role's `defaults` and `vars` are visible in the
    /// play's `pre_tasks`, which run **before** the role, in its `tasks` and `post_tasks`, and
    /// inside the other roles - a role early in the list sees a variable only a later one sets.
    /// So this is one map per layer for the whole play, not something that accumulates as the
    /// list is walked, and a role's own values are laid over it for that role's own steps.
    pub exported: RoleVars,
    /// Every handler the play can reach, in the order they run: each role's, in the order the
    /// roles run, then the play's own. Measured on ansible-core 2.19.12 - handlers run in
    /// **definition** order and not in notification order, and a role's run before the play's at
    /// the same flush.
    ///
    /// An `include_role` appends its role's handlers **behind** the play's own rather than in
    /// front of them, measured: a play handler and an included role's handler notified in that
    /// order run in that order, which is the opposite of what the `roles:` list and `import_role`
    /// give. Appending is also what keeps every index already in a host's notification list
    /// pointing at the handler it was pointing at.
    pub handlers: Vec<Handler>,
    /// The play's own keywords, the outermost layer of every task's merge. An include whose step
    /// carries no [`Origin::inherited`] of its own is compiled under it.
    pub inherited: PlayTask,
    /// Where a role named at run time is looked for. Kept with the compiled play because an
    /// `include_role` resolves its role while the play runs and the search list is the play's.
    pub search: RoleSearch,
    /// The tag selection this run was narrowed to, for the same reason: the tasks an include
    /// brings in are filtered by `--tags` the way the compiled ones were.
    pub selection: TagSelection,
    /// Whether `steps[0]` is the `Gathering Facts` step the play's `gather_facts` put there
    /// rather than anything the playbook wrote.
    ///
    /// It runs and reports like every other task - the reference shows its banner, counts it `ok`
    /// in the recap and lets `--skip-tags always` drop it - and it is left out of the four
    /// listings alone, which is where the reference leaves it out too.
    pub gathers: bool,
    /// Per step, whether it reads what another host did, so the hosts meet in front of it under
    /// `[volant] batching`. Filled by the executor's `mark_boundaries` once the list is laid out,
    /// and again behind every splice, because a role an include brings in brings its own
    /// variables. A list out of step with `steps` - never filled - makes every step a boundary:
    /// that costs time, and never lets a host read ahead of the host it reads.
    pub crosses: Vec<bool>,
    /// What ansible-core said about the groups and keys of `module_defaults`, shared by the plays
    /// of a run and by the includes read while one runs. The run fills it before the first
    /// connection; a name an include brings in later is asked about when its task is prepared.
    pub resolved: Arc<crate::collection::Resolved>,
    /// What compiling the play had to say, for the run to print before the first banner.
    pub warnings: Vec<String>,
    /// `--start-at-task`: the step every host starts at, the steps in front of it being left out
    /// as the reference's iterator leaves them. The length of the list when the task is in a
    /// later play, so this one runs nothing. The `Gathering Facts` step still runs.
    pub start_at: Option<usize>,
}
