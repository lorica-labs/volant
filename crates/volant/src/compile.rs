// SPDX-License-Identifier: GPL-3.0-or-later
//! Turns a play into the flat list of steps the executor walks.
//!
//! A play is written as a tree - blocks inside blocks, each passing its keywords down - and run
//! as a sequence. Flattening it once, before the first connection, means every host walks the
//! same numbered list, and the coordinator can talk about "step 7" to all of them at once. What
//! the tree said about grouping survives as a span per block plus the section each step belongs
//! to, and [`after`] reads those to decide where a host goes next.
//!
//! The dangerous half of a flattening is the step nobody runs: an index skipped silently is a
//! run that reports success having done less than the playbook asked for. Two things guard it.
//! Nothing here invents an index - every one returned names a step this compilation laid out -
//! and the driver tells the coordinator about every index it steps over, so a step left out is
//! a step the recap can still account for.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;
use serde_json::{Map, Value};
use volant_protocol::modules::short_name;

use crate::inventory::Host;
use crate::playbook::{Block, Play, PlayTask, TaskOrBlock};
use crate::roles::{RoleEntry, RoleSearch, RoleVars};
use crate::stats::Refusal;
use crate::template::Templar;

/// Which tasks `--tags` and `--skip-tags` leave in.
///
/// This is ansible-core's own tag algebra, read off `Taggable.evaluate_tags` on 2.19.12 and
/// measured case by case against it. Three things in it are easy to get wrong and each one was
/// measured: a task carrying `always` survives every `--tags`, and survives `--skip-tags all`
/// unless `always` is itself skipped; a task carrying `never` is left out of `--tags all` and of
/// `--tags tagged`, and comes back only when something names it; and a task carrying no tag at
/// all is not an empty set there but the one-element set `{untagged}`, which is why
/// `--tags untagged` selects it and `--skip-tags untagged` drops it.
///
/// One selection serves the run and the four listings, so `--tags x` cannot show one list and
/// run another.
#[derive(Debug, Clone)]
pub(crate) struct TagSelection {
    run: Vec<String>,
    skip: Vec<String>,
}

/// The selection a run nobody narrowed gets, built through [`TagSelection::new`] rather than from
/// two empty lists: an empty `run` list selects **nothing**, so a derived `Default` would give a
/// compiled play that drops every task it was handed.
impl Default for TagSelection {
    fn default() -> Self {
        TagSelection::new(Vec::new(), Vec::new())
    }
}

/// What the reference calls a task with no tags of its own.
const UNTAGGED: &str = "untagged";

impl TagSelection {
    /// An empty `--tags` is the pseudo-tag `all`, which the reference substitutes before a play
    /// ever sees the list. Doing it in the constructor rather than at each call site is what
    /// keeps `never` out of a run nobody narrowed: with an empty list the first half of
    /// [`TagSelection::selects`] does not run at all, and a `never` task would stay in.
    pub fn new(run: Vec<String>, skip: Vec<String>) -> Self {
        TagSelection {
            run: if run.is_empty() {
                vec!["all".to_string()]
            } else {
                run
            },
            skip,
        }
    }

    /// The tags asked for and the tags skipped, as `ansible_run_tags` and `ansible_skip_tags`
    /// show them: `['all']` and `[]` when nothing narrowed the run.
    pub fn lists(&self) -> (&[String], &[String]) {
        (&self.run, &self.skip)
    }

    pub fn selects(&self, tags: &[String]) -> bool {
        let own: &[String] = tags;
        let untagged = own.is_empty() || (own.len() == 1 && own[0] == UNTAGGED);
        // An empty tag list reads as `{untagged}`, exactly as it does in the reference.
        let carries = |name: &str| {
            if own.is_empty() {
                name == UNTAGGED
            } else {
                own.iter().any(|t| t == name)
            }
        };
        let names = |list: &[String]| list.iter().any(|t| carries(t));
        let listed = |list: &[String], name: &str| list.iter().any(|t| t == name);

        // No emptiness test on the run list: `new` put `all` in an empty one, so the selecting
        // half always applies. Guarding it would be a branch that cannot be taken and would read
        // as if the dangerous value were still reachable.
        let mut run = carries("always")
            || (listed(&self.run, "all") && !carries("never"))
            || names(&self.run)
            || (listed(&self.run, "tagged") && !untagged && !carries("never"));
        if run && !self.skip.is_empty() {
            if listed(&self.skip, "all") {
                // `--skip-tags all` is the one place `always` survives a skip, and it stops
                // surviving the moment `always` is named alongside it.
                run = carries("always") && !listed(&self.skip, "always");
            } else if names(&self.skip) || (listed(&self.skip, "tagged") && !untagged) {
                run = false;
            }
        }
        run
    }
}

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

impl Compiled {
    /// Inserts `steps` at `at`, growing every block range that ends at or after `at` and shifting
    /// every range that starts at or after it.
    ///
    /// Called by the coordinator alone, at a flush point, once every host has reported that step
    /// and before any of them reads the list again - which is what keeps the index each driver
    /// holds pointing at the step it was pointing at. A range that *ends* at `at` has to grow:
    /// `at` is one past the flush, so a flush written last in a block's body ends that body
    /// exactly there, and testing `>` instead of `>=` would drop the spliced handlers into that
    /// block's rescue, where a host that failed nothing never goes.
    pub(crate) fn splice(&mut self, at: usize, steps: Vec<Step>) {
        let n = steps.len();
        for span in &mut self.blocks {
            for range in [&mut span.body, &mut span.rescue, &mut span.always] {
                if range.start >= at {
                    range.start += n;
                }
                if range.end >= at {
                    range.end += n;
                }
            }
        }
        // Marked as boundaries until the next `mark_boundaries` says otherwise, so a splice
        // nobody marks is the strict `linear` and never a host running ahead.
        if self.crosses.len() == self.steps.len() {
            self.crosses.splice(at..at, std::iter::repeat_n(true, n));
        }
        self.steps.splice(at..at, steps);
    }
}

/// The file a task statement's relative name points at: the one beside the file that wrote the
/// statement, and failing that the one in the role's own `tasks/`.
///
/// Measured on ansible-core 2.19.12 with a role whose `tasks/sub/deep.yml` names `shared.yml`, in
/// all three states - only `tasks/shared.yml`, only `tasks/sub/shared.yml`, both - and the one
/// beside the writer wins whenever it is there. The fallback is what a role like UBUNTU22-CIS
/// needs: `tasks/section_1/cis_1.1.1.x.yml` writes `file: warning_facts.yml` and means
/// `tasks/warning_facts.yml`.
///
/// One function for `import_tasks` and `include_tasks` both: the static and the dynamic statement
/// read the same name the same way, and two copies of this rule would be two chances to move only
/// one of them. The path it returns is the one to report as missing when nothing is there, which
/// is why it answers with a path rather than an `Option`.
pub(crate) fn beside_or_in_role(file_dir: &Path, role_dir: Option<&Path>, name: &str) -> PathBuf {
    let beside = file_dir.join(name);
    match role_dir.map(|role| role.join("tasks").join(name)) {
        Some(in_role) if !beside.is_file() && in_role.is_file() => in_role,
        _ => beside,
    }
}

/// The handlers one name notifies, in definition order: the **first** one carrying it as its
/// `name`, and **every** one listening for it.
///
/// The asymmetry is measured on ansible-core 2.19.12, both halves on their own: two handlers
/// written with the same name run the first alone, and a handler named `x` beside one listening
/// for `x` both run when `x` is notified. It is what makes a role that runs three times
/// contribute one handler rather than three - the three copies share a name, and only the first
/// of them is ever reached.
///
/// `names` is what each handler's `name` renders to for the host asking, one per handler in
/// order, `None` for one that does not render: a handler name may be a template, `listen` never is.
pub(crate) fn resolve_notify(c: &Compiled, name: &str, names: &[Option<String>]) -> Vec<usize> {
    let named = names.iter().position(|n| n.as_deref() == Some(name));
    c.handlers
        .iter()
        .enumerate()
        .filter(|(i, h)| named == Some(*i) || h.listen.iter().any(|l| l == name))
        .map(|(i, _)| i)
        .collect()
}

/// The steps a flush point at `at` inserts behind itself: one per handler, in definition order.
///
/// All of them, not only the notified ones, because the coordinator decides this for every host
/// at once and which handlers a host notified is the driver's own business. A step for a handler
/// the host did not notify reports nothing and shows nothing.
///
/// They carry the flush step's own block and section, which is what makes the spliced steps part
/// of the section the flush was written in: measured on ansible-core 2.19.12, a handler that
/// fails at a flush written inside a block is taken by that block's `rescue` (`rescued=1`, exit
/// 0), and a host that never entered a rescue holding the flush steps over the handlers too.
///
/// And they carry its mask, for the same reason. A `meta: flush_handlers` inside an
/// `include_tasks` only one host asked for is spliced in for that host alone; handler steps left
/// unmasked behind it would be walked by every host of the batch, and each of them would run the
/// handlers **it** had notified, at a flush it was never asked to perform.
pub(crate) fn handler_steps(c: &Compiled, at: usize) -> Vec<Step> {
    let (block, section) = (c.steps[at].block, c.steps[at].section);
    c.handlers
        .iter()
        .enumerate()
        .map(|(i, h)| Step {
            kind: StepKind::Handler(i),
            task: h.task.clone(),
            block,
            section,
            role: h.role,
            origin: Arc::clone(&c.steps[at].origin),
            include_params: c.steps[at].include_params.clone(),
            hosts: c.steps[at].hosts.clone(),
        })
        .collect()
}

/// Every `meta` action ansible-core 2.19.12 accepts. `flush_handlers` compiles into a flush point of
/// its own; the executor runs the rest where the step stands.
pub(crate) const META_ACTIONS: &[&str] = &[
    "clear_facts",
    "clear_host_errors",
    "end_batch",
    "end_host",
    "end_play",
    "end_role",
    "flush_handlers",
    "noop",
    "refresh_inventory",
    "reset_connection",
];

/// The `meta` action that runs the notified handlers where it stands.
pub(crate) const FLUSH_HANDLERS: &str = "flush_handlers";

/// The action a `meta` task asked for.
pub(crate) fn meta_action(task: &PlayTask) -> &str {
    task.args
        .get("_raw_params")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The play's hosts cut into the batches `serial` asks for, in inventory order.
///
/// This is `PlaybookExecutor._get_serialized_batches` as ansible-core 2.19.12 writes it, and
/// three of its properties were measured rather than derived. A percentage is taken of the
/// **whole** host list and never of what is left, so `[1, "50%"]` on three hosts gives three
/// batches of one rather than one and then two. A size that comes out at or below zero means
/// every remaining host in one batch and ends the cutting there, which is how `serial: 0`,
/// `serial: -1`, `serial: []` and a `serial` larger than the inventory all end up as the single
/// batch this engine ran before this keyword existed. And a percentage that truncates to zero is
/// one host, not none: `"10%"` of three hosts is one, so a small percentage cannot silently ask
/// for a batch of nobody.
///
/// The hosts handed in are the play's **resolved** hosts, failures of earlier plays included.
/// Measured: after `h1` fails, a second play over `all` with `serial: 2` still cuts `[h1, h2]`
/// and `[h3]`, and prints a banner for each - filtering the failed host out first would merge
/// the two batches into one and run `h2` and `h3` together, which is not what the reference does.
pub(crate) fn batches(
    hosts: &[Host],
    serial: Option<&Value>,
    templar: &Templar,
    vars: &Map<String, Value>,
) -> anyhow::Result<Vec<Vec<Host>>> {
    // The reference stores `serial` as a list, so a scalar is a one-element list and an empty
    // list is `[-1]`, which is "every host at once". An absent `serial` reaches the same place.
    let rendered = match serial {
        None => Value::Null,
        // Measured, exit 4: `serial: "{{ n }}"` with no `n` says `Error processing keyword
        // 'serial': 'n' is undefined` and stops the run there, before the play's banner.
        Some(raw) => templar.render_value(raw, vars).map_err(|err| {
            Refusal::at(4, format!("Error processing keyword 'serial': {}", err.0))
        })?,
    };
    let sizes: Vec<Value> = match rendered {
        Value::Null => Vec::new(),
        Value::Array(values) => values,
        other => vec![other],
    };
    let total = hosts.len();
    let mut out: Vec<Vec<Host>> = Vec::new();
    let mut rest = hosts;
    let mut item = 0;
    while !rest.is_empty() {
        // Past the end of the list the last element repeats, which is what makes `serial: [1]`
        // and `serial: 1` the same request.
        let size = match sizes.get(item.min(sizes.len().saturating_sub(1))) {
            None => -1,
            Some(value) => serial_size(value, total)?,
        };
        if size <= 0 {
            out.push(rest.to_vec());
            break;
        }
        let size = (size as usize).min(rest.len());
        out.push(rest[..size].to_vec());
        rest = &rest[size..];
        item += 1;
    }
    Ok(out)
}

/// One `serial` element as a number of hosts, following `ansible.utils.helpers.pct_to_int`:
/// `int(value)` for anything but a percentage, and `int(pct / 100 * total) or 1` for one.
///
/// `int()` truncates towards zero rather than rounding, and its `or` turns a zero into one; a
/// negative stays negative and the caller reads it as "all of them". Returned signed for that
/// reason: clamping to zero here would lose the difference between `serial: 0`, which is every
/// host, and a batch of none, which would spin this loop for ever.
fn serial_size(value: &Value, total: usize) -> anyhow::Result<i64> {
    // The reference lets Python's own `ValueError` out here: it prints `Unexpected Exception,
    // this is probably a bug: invalid literal for int() with base 10: 'abc'` with a traceback and
    // exits 250. Measured, and a deliberate divergence: the sentence is kept, the crash is not,
    // so a `serial` nobody can read is refused at exit 4 like the rest of an unusable play.
    let refused = |value: &Value| -> anyhow::Error {
        let shown = value
            .as_str()
            .map_or_else(|| crate::render::ansible_json(value), str::to_string);
        Refusal::at(
            4,
            format!(
                "Error processing keyword 'serial': invalid literal for int() with base 10: \
                 '{shown}'"
            ),
        )
    };
    match value {
        // `int(2.7)` is 2 there, and `int(True)` is 1.
        Value::Number(n) => Ok(n.as_f64().unwrap_or_default().trunc() as i64),
        Value::Bool(b) => Ok(i64::from(*b)),
        // The reference tests `value.endswith('%')` on the **raw** string, trailing whitespace
        // included: `"50% "` (a space after the sign) fails that test there, falls to `int()`,
        // and crashes the same way `serial: abc` does. `trim_end()` here used to accept it as a
        // percentage instead of refusing it - measured divergence, fixed by matching the raw
        // string like the reference does.
        Value::String(s) if s.ends_with('%') => {
            let pct: i64 = s
                .trim()
                .trim_end_matches('%')
                .trim()
                .parse()
                .map_err(|_| refused(value))?;
            let size = (pct as f64 / 100.0 * total as f64).trunc() as i64;
            Ok(if size == 0 { 1 } else { size })
        }
        Value::String(s) => s.trim().parse::<i64>().map_err(|_| refused(value)),
        other => Err(refused(other)),
    }
}

/// How deep roles and imported files may nest before the run is refused: the same ceiling a
/// cycle through an inventory's `:children` is given, for the same reason - a cycle here reads
/// files and recurses on what it finds until the stack is gone.
///
/// One counter for all three recursions - a `meta/main.yml` dependency, an `import_role` inside
/// a role's tasks, an `import_tasks` inside an imported file - because a cycle can run through
/// any mixture of them, and a counter that only one of them increments bounds nothing.
pub(crate) const DEPTH: usize = 32;

/// What the reference says when two roles import each other, measured on ansible-core 2.19.12,
/// exit 1.
///
/// A file importing itself with `import_tasks` is not caught there at all: the interpreter runs
/// out of stack and `ansible-playbook` exits **250** printing a Python traceback under
/// `Unexpected Exception, this is probably a bug`. This engine refuses that shape too, at exit 1
/// and in its own words - the divergence is deliberate, since 250 is the reference reporting its
/// own crash and there is no traceback here to go with it.
const RECURSION: &str = "A recursion loop was detected with the roles specified. Make sure child roles do not have dependencies on parent roles: maximum recursion depth exceeded";

/// The role instances a play has already run.
///
/// The identity of a role instance is **not** its name: measured on ansible-core 2.19.12,
/// `- base` written twice runs once, while `- base` and `- { role: base, p: v }` run twice, and
/// so do `- base` and `- { role: base, vars: { p: v } }`. So everything the entry says about what
/// the role will do goes into the identity - its parameters, its `vars:`, its `when:` and which
/// files of the role it asked for. Comparing names alone drops the second entry, and a run that
/// skipped a role the playbook asked for reports success having done less than it says.
type Seen = Vec<(String, Value)>;

fn identity(entry: &RoleEntry) -> (String, Value) {
    (
        entry.name.clone(),
        serde_json::json!({
            "params": sorted(&entry.params),
            "vars": sorted(&entry.keywords.vars),
            "when": entry.keywords.when,
            "from": [
                &entry.from.tasks,
                &entry.from.vars,
                &entry.from.defaults,
                &entry.from.handlers,
            ],
        }),
    )
}

/// A mapping as a value that compares by content rather than by the order the YAML happened to
/// list the keys in: two entries writing the same parameters in a different order are the same
/// entry.
fn sorted(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut pairs: Vec<(&String, &Value)> = map.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
}

struct Builder<'a> {
    steps: Vec<Step>,
    blocks: Vec<BlockSpan>,
    roles: Vec<RoleVars>,
    /// Every role's handlers, in the order the roles run. The play's own are appended behind
    /// them, once, when the walk is over.
    handlers: Vec<Handler>,
    search: &'a RoleSearch,
    /// The directory relative paths are read against: the playbook's, then a role's `tasks/`
    /// while its tasks are being compiled, then the directory of an imported file while its own
    /// imports are being read. Measured: `import_tasks: sub/extra.yml` inside a role's
    /// `tasks/main.yml` reads `<role>/tasks/sub/extra.yml`, and an `import_tasks` inside **that**
    /// file reads it beside itself.
    file_dir: PathBuf,
    /// The root of the role being compiled, if any. Its `tasks/` is the first place an
    /// `import_tasks` written inside a role looks, and its `vars/` the first place an
    /// `include_vars` does.
    role_dir: Option<PathBuf>,
    /// How many roles and imported files are open above whatever is being compiled now.
    depth: usize,
    /// How many includes deep the statement that started this compilation was. Zero for a play;
    /// one more than its parent for everything an include splices in.
    include_depth: u32,
    /// Which tasks `--tags` and `--skip-tags` leave in. A task the selection drops is never
    /// pushed, rather than pushed and filtered out afterwards: a `retain` over the finished
    /// list would leave every `BlockSpan` pointing at the indices the list used to have, and a
    /// span one step wide of the wrong step is how a host walks into a section it never entered.
    selection: &'a TagSelection,
}

impl Builder<'_> {
    /// The one place a step is added, so the tag selection cannot be honoured on one path and
    /// forgotten on another.
    fn push(&mut self, step: Step) {
        if self.selection.selects(&step.task.tags) {
            self.steps.push(step);
        }
    }

    /// Where the steps being laid out now were written, shared by all of them.
    fn origin(&self) -> Arc<Origin> {
        Arc::new(Origin {
            file_dir: self.file_dir.clone(),
            role_dir: self.role_dir.clone(),
            depth: self.include_depth,
            inherited: None,
        })
    }

    /// One of the three flush points the compiler puts in itself, at the end of a section that
    /// has something in it: `mark` is where the section started, so a section that laid out no
    /// step gets no flush point. Nothing there could have notified anything, and a step every
    /// host has to report through is a barrier.
    ///
    /// Pushed past [`Builder::push`] on purpose: it is the engine's own structure and not
    /// something the playbook wrote, so no `--tags` may drop it. A flush point the selection
    /// removed would leave the handlers a selected task notified with nowhere to run.
    fn flush_point(&mut self, mark: usize) {
        if self.steps.len() == mark {
            return;
        }
        self.steps.push(Step {
            kind: StepKind::Flush { explicit: false },
            task: PlayTask::empty(),
            block: None,
            section: Section::Body,
            role: None,
            origin: self.origin(),
            include_params: None,
            hosts: None,
        });
    }

    fn items(
        &mut self,
        items: &[TaskOrBlock],
        inherited: &PlayTask,
        block: Option<usize>,
        section: Section,
        role: Option<usize>,
    ) -> anyhow::Result<()> {
        for item in items {
            match item {
                TaskOrBlock::Task(task) => self.task(task, inherited, block, section, role)?,
                TaskOrBlock::Block(inner) => {
                    self.block(inner, inherited, block, section, role)?;
                }
            }
        }
        Ok(())
    }

    /// One task, unless it is one of the three import statements - those name work rather than
    /// being it, and what they name is spliced here, where the task would have gone.
    fn task(
        &mut self,
        task: &PlayTask,
        inherited: &PlayTask,
        block: Option<usize>,
        section: Section,
        role: Option<usize>,
    ) -> anyhow::Result<()> {
        match short_name(&task.module) {
            "import_tasks" => return self.import_tasks(task, inherited, block, section, role),
            "import_role" => {
                let entry = import_role_entry(task)?;
                // A fresh `seen` list: measured, `import_role` runs its role even when the
                // play's `roles:` list has already run it with the same parameters. Its
                // dependencies still deduplicate against each other.
                return self.role(
                    &entry,
                    &merge(inherited, task),
                    &mut Vec::new(),
                    block,
                    section,
                );
            }
            // Measured: written as a task rather than in the list of plays, the reference fails
            // the task with `Action 'ansible.builtin.import_playbook' does not support raw
            // params.`, exit 2. This refuses before the play instead of during it, so nothing
            // has run when the operator reads it, but the code is the one their scripts read.
            "import_playbook" => {
                return Err(Refusal::at(
                    2,
                    "Task failed: Action 'ansible.builtin.import_playbook' does not support raw params.",
                ));
            }
            _ => {}
        }
        let task = merge(inherited, task);
        let kind = match crate::playbook::is_meta(&task) {
            true if meta_action(&task) == FLUSH_HANDLERS => StepKind::Flush { explicit: true },
            true => StepKind::Meta,
            // The two dynamic statements become a step of their own rather than a task: nothing
            // runs them on a host, and the arguments they carry are refused here, before the
            // first connection, so an option this release cannot honour cannot be read and
            // dropped while the play is already half applied.
            false => match short_name(&task.module) {
                "include_tasks" => {
                    include_options("include_tasks", &task.args)?;
                    StepKind::Include(IncludeKind::Tasks)
                }
                "include_role" => {
                    include_role_options(&task.args)?;
                    StepKind::Include(IncludeKind::Role)
                }
                // A module like any other, but one whose arguments name work on the controller
                // rather than on a host, so what it can be told is checked with the statements.
                "include_vars" => {
                    include_vars_options(&task.args)?;
                    StepKind::Task
                }
                _ => StepKind::Task,
            },
        };
        let mut origin = self.origin();
        if matches!(kind, StepKind::Include(_)) {
            Arc::make_mut(&mut origin).inherited = Some(Arc::new(inherited.clone()));
        }
        self.push(Step {
            kind,
            task,
            block,
            section,
            role,
            origin,
            include_params: None,
            hosts: None,
        });
        Ok(())
    }

    /// `import_tasks`: the file's own task list, compiled where the statement stands, with the
    /// statement's keywords folded into every task of it.
    fn import_tasks(
        &mut self,
        task: &PlayTask,
        inherited: &PlayTask,
        block: Option<usize>,
        section: Section,
        role: Option<usize>,
    ) -> anyhow::Result<()> {
        // Measured, exit 4: the reference refuses this before anything runs rather than looping
        // over a statement that was resolved once at compile time.
        if task.loop_items.is_some() {
            bail!(
                "You cannot use loops on 'import_tasks' statements. You should use 'include_tasks' instead."
            );
        }
        unknown_options("import_tasks", &task.args, &["file", "_raw_params"], &[])?;
        let name = task
            .args
            .get("file")
            .or_else(|| task.args.get("_raw_params"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("'import_tasks' takes a file name"))?;
        // Inside a role a bare name has two homes and the reference takes the first that exists:
        // the directory of the file that wrote the statement, then the role's own `tasks/`.
        // Measured on ansible-core 2.19.12 with a role whose `tasks/sub/deep.yml` imports
        // `shared.yml`, in all three states - only `tasks/shared.yml`, only `tasks/sub/
        // shared.yml`, both - and the one beside the importer wins whenever it is there. The
        // fallback is what a role like UBUNTU22-CIS needs: `tasks/section_1/cis_1.1.1.x.yml`
        // writes `import_tasks: file: warning_facts.yml` and means `tasks/warning_facts.yml`,
        // so trying the importer's directory alone refuses a role the reference compiles.
        let path = beside_or_in_role(&self.file_dir, self.role_dir.as_deref(), name);
        if !path.is_file() {
            // Exit 1, measured, with the reference's own two sentences. The reference adds a
            // third naming Python's own errno, which this engine has nothing to say about.
            return Err(Refusal::at(
                1,
                format!(
                    "Unable to retrieve file contents.\nCould not find or access '{}' on the Ansible Controller.",
                    path.display()
                ),
            ));
        }
        // A file that imports itself, directly or around a ring of files, otherwise reads and
        // recurses until the stack is gone: no recap, no exit code, nothing to read. Refused
        // here instead, at the depth every other recursion in this compilation shares.
        self.depth += 1;
        if self.depth > DEPTH {
            return Err(Refusal::at(
                1,
                format!(
                    "imports nest deeper than {DEPTH} levels at '{}': a file that imports itself",
                    path.display()
                ),
            ));
        }
        let imported = crate::playbook::parse_tasks_file(&path)?;
        let merged = merge(inherited, task);
        // The imported file's own directory, for the imports it carries itself, put back
        // afterwards so the rest of the importing file still reads against its own.
        let previous = std::mem::replace(
            &mut self.file_dir,
            path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        );
        let result = self.items(&imported, &merged, block, section, role);
        self.file_dir = previous;
        self.depth -= 1;
        result
    }

    /// One role entry: its dependencies first, then its argument-spec check, then its tasks.
    ///
    /// Measured on ansible-core 2.19.12: a `meta/main.yml` dependency runs before the role that
    /// depends on it, with its own parameters, and a dependency two roles share runs once.
    fn role(
        &mut self,
        entry: &RoleEntry,
        inherited: &PlayTask,
        seen: &mut Seen,
        block: Option<usize>,
        section: Section,
    ) -> anyhow::Result<()> {
        let path = self.search.locate(&entry.name)?;
        let role = crate::roles::load(&path, &entry.from)?;
        let key = identity(entry);
        if !role.allow_duplicates && seen.contains(&key) {
            return Ok(());
        }
        seen.push(key);
        // Two roles that import each other never repeat an identity - `import_role` starts a
        // `seen` list of its own, as measured - so the dedup above stops nothing and only the
        // depth does. The reference's own sentence, at the exit 1 measured with it.
        self.depth += 1;
        if self.depth > DEPTH {
            return Err(Refusal::at(1, RECURSION));
        }
        let mut kw = merge(inherited, &entry.keywords);
        // A role does not inherit the play's `collections`: it searches its own `meta/main.yml`
        // list, which may be empty, and a block or task inside it may still write another.
        kw.collections = Some(role.collections.clone());
        for dependency in &role.dependencies {
            self.role(dependency, &kw, seen, block, section)?;
        }
        let index = self.roles.len();
        self.roles.push(RoleVars {
            name: entry.name.clone(),
            defaults: role.defaults,
            vars: role.vars,
            params: entry.params.clone(),
        });
        // The role's handlers, before its tasks are walked, so a role imported by another one
        // contributes its handlers in front of the importer's. Their keywords are merged the way
        // a task's are: a `when` or a `become` written on the role entry reaches them too.
        for handler in role.handlers {
            self.handlers.push(Handler {
                task: merge(&kw, &handler.task),
                listen: handler.listen,
                role: Some(index),
            });
        }
        if let Some(spec) = role.argument_spec {
            let task = merge(
                &kw,
                &validation(entry, &path, spec, role.argument_spec_description),
            );
            self.push(Step {
                kind: StepKind::Task,
                task,
                block,
                section,
                role: Some(index),
                origin: self.origin(),
                include_params: None,
                hosts: None,
            });
        }
        let previous = std::mem::replace(&mut self.file_dir, path.join("tasks"));
        let outer_role = self.role_dir.replace(path);
        let result = self.items(&role.tasks, &kw, block, section, Some(index));
        self.file_dir = previous;
        self.role_dir = outer_role;
        self.depth -= 1;
        result
    }

    fn block(
        &mut self,
        b: &Block,
        inherited: &PlayTask,
        parent: Option<usize>,
        section: Section,
        role: Option<usize>,
    ) -> anyhow::Result<()> {
        let merged = merge(inherited, &b.keywords);
        // Reserved before the sections are walked, so a block nested inside this one gets an
        // identifier of its own and can name this one as its parent.
        let id = self.blocks.len();
        self.blocks.push(BlockSpan {
            body: 0..0,
            rescue: 0..0,
            always: 0..0,
            parent,
            section,
        });
        let start = self.steps.len();
        self.items(&b.body, &merged, Some(id), Section::Body, role)?;
        let rescue = self.steps.len();
        self.items(&b.rescue, &merged, Some(id), Section::Rescue, role)?;
        let always = self.steps.len();
        self.items(&b.always, &merged, Some(id), Section::Always, role)?;
        let end = self.steps.len();
        self.blocks[id].body = start..rescue;
        self.blocks[id].rescue = rescue..always;
        self.blocks[id].always = always..end;
        Ok(())
    }
}

/// The check the reference inserts in front of a role that ships `meta/argument_specs.yml`.
///
/// Measured: the banner reads `TASK [<role> : Validating arguments against arg spec '<entry>']`,
/// the task runs on the controller, and a failure carries `argument_errors`,
/// `argument_spec_data` and a `validate_args_context` naming the role and its absolute path.
/// A spec carrying a `short_description` puts it after the entry name behind a dash, measured
/// through `--list-tasks`: `Validating arguments against arg spec 'main' - The spec role`.
fn validation(
    entry: &RoleEntry,
    path: &Path,
    spec: Map<String, Value>,
    description: Option<String>,
) -> PlayTask {
    let name = match description {
        Some(text) => format!(
            "Validating arguments against arg spec '{}' - {text}",
            entry.from.tasks
        ),
        None => format!(
            "Validating arguments against arg spec '{}'",
            entry.from.tasks
        ),
    };
    let mut args = Map::new();
    args.insert("argument_spec".to_string(), Value::Object(spec));
    args.insert(
        "provided_arguments".to_string(),
        Value::Object(entry.params.clone()),
    );
    args.insert(
        "validate_args_context".to_string(),
        serde_json::json!({
            "argument_spec_name": entry.from.tasks,
            "name": entry.name,
            "path": path.display().to_string(),
            "type": "role",
        }),
    );
    PlayTask {
        name,
        named: true,
        module: "validate_argument_spec".to_string(),
        args,
        // Measured on ansible-core 2.19.12: the check lists as
        // `TAGS: [always, <the role entry's tags>]`, so it carries `always` and runs whatever
        // `--tags` asks for - a role's arguments are validated before its tasks or not at all.
        tags: vec!["always".to_string()],
        ..PlayTask::empty()
    }
}

/// Refuses an argument the statement does not take, and one it takes but this release cannot
/// honour.
///
/// Measured on ansible-core 2.19.12: `import_role: { name: x, typo: v }` is refused with
/// `Invalid options for import_role: typo`, exit 4, and `import_tasks` with `apply:` the same
/// way. An argument read and dropped is the failure family this engine is written against - the
/// run would report success having done something other than what was asked - so the ones the
/// reference accepts and this release has nothing to do with are refused by their own names
/// rather than ignored.
fn unknown_options(
    statement: &str,
    args: &Map<String, Value>,
    known: &[&str],
    unsupported: &[&str],
) -> anyhow::Result<()> {
    let mut invalid: Vec<&str> = args
        .keys()
        .map(String::as_str)
        .filter(|key| !known.contains(key))
        .collect();
    invalid.sort_unstable();
    if !invalid.is_empty() {
        bail!("Invalid options for {statement}: {}", invalid.join(", "));
    }
    if let Some(key) = unsupported.iter().find(|key| args.contains_key(**key)) {
        bail!("'{statement}' option '{key}' is not supported yet");
    }
    Ok(())
}

/// The role one `import_role` task names, read out of its arguments.
fn import_role_entry(task: &PlayTask) -> anyhow::Result<RoleEntry> {
    // Measured: the reference takes these nine and refuses anything else. The last three name
    // work this release does not do - nothing dedups an `import_role`, the argument-spec check
    // has no off switch, and `public` exports nothing of its own - so they are refused rather
    // than accepted and forgotten. `public` is refused by value and not by presence: see
    // `refuse_public`.
    unknown_options(
        "import_role",
        &task.args,
        &[
            "name",
            "role",
            "tasks_from",
            "vars_from",
            "defaults_from",
            "handlers_from",
            "public",
            "allow_duplicates",
            "rolespec_validate",
        ],
        &["allow_duplicates", "rolespec_validate"],
    )?;
    refuse_public("import_role", &task.args)?;
    let text = |key: &str| task.args.get(key).and_then(Value::as_str);
    let name = text("name")
        .or_else(|| text("role"))
        .ok_or_else(|| anyhow::anyhow!("'import_role' takes a role name"))?;
    let from = |key: &str, fallback: &str| text(key).unwrap_or(fallback).to_string();
    Ok(RoleEntry {
        name: name.to_string(),
        from: crate::roles::RoleFrom {
            tasks: from("tasks_from", "main"),
            vars: from("vars_from", "main"),
            defaults: from("defaults_from", "main"),
            handlers: from("handlers_from", "main"),
        },
        // An `import_role` takes no free keys: everything it says is an argument of the
        // statement, and what it hands the role travels in the task's own `vars:`, which the
        // merge already carries down to every task of the role.
        params: Map::new(),
        keywords: PlayTask::empty(),
    })
}

/// The play's `gather_subset`, `gather_timeout` and `fact_path` as the arguments of its `setup`.
///
/// A `gather_subset` written as one plain string is a one-item list, never split on commas:
/// measured on ansible-core 2.19.12, `"!all,!min,local"` names nothing the module knows and
/// leaves `min`. A template stays as written, and the play start refuses one that renders to a
/// string with a comma in it, which the module would split where the reference does not.
fn gather_args(play: &Play) -> Map<String, Value> {
    let mut args = Map::new();
    if let Some(subset) = &play.gather_subset {
        let subset = match subset {
            Value::String(text) if !Templar::is_template(text) => {
                Value::Array(vec![subset.clone()])
            }
            other => other.clone(),
        };
        args.insert("gather_subset".into(), subset);
    }
    for (name, value) in [
        ("gather_timeout", &play.gather_timeout),
        ("fact_path", &play.fact_path),
    ] {
        if let Some(value) = value {
            args.insert(name.into(), value.clone());
        }
    }
    args
}

/// A play's sections, flattened into one numbered list.
///
/// The order is the reference's own, measured: `pre_tasks`, then the roles with each one's
/// dependencies in front of it, then `tasks`, then `post_tasks`.
///
/// The play's own `become` and `become_user` are deliberately left out of the merge: their
/// precedence against a host variable was measured in an earlier release and lives in the
/// executor, which reads them from the play. A block is a task keyword, so its `become` does
/// come down here.
pub(crate) fn compile(
    play: &Play,
    search: &RoleSearch,
    selection: &TagSelection,
) -> anyhow::Result<Compiled> {
    let mut builder = Builder {
        steps: Vec::new(),
        blocks: Vec::new(),
        roles: Vec::new(),
        handlers: Vec::new(),
        search,
        file_dir: play.dir.clone(),
        role_dir: None,
        depth: 0,
        include_depth: 0,
        selection,
    };
    // The play's own tags are the outermost layer of the merge, so every task under it - in a
    // role, in an imported file, at any block depth - carries them for the selection and for
    // the listing. Measured: `--skip-tags <play tag>` leaves a play with nothing to do.
    // `no_log`, `environment`, `check_mode`, `diff` and `run_once` join the play's tags here: all
    // are task keywords a play may also carry, so putting them in the outermost layer of the
    // merge is what
    // makes a play-wide `environment` reach a task inside a role inside a block, with no second
    // path to keep in step with this one.
    let empty = PlayTask {
        tags: play.tags.clone(),
        no_log: play.no_log,
        environment: play.environment.clone(),
        module_defaults: play.module_defaults.clone(),
        collections: play.collections.clone(),
        check_mode: play.check_mode.clone(),
        diff: play.diff.clone(),
        run_once: play.run_once,
        timeout: play.timeout,
        throttle: play.throttle,
        debugger: play.debugger,
        any_errors_fatal: play.any_errors_fatal,
        ignore_errors: play.ignore_errors.clone(),
        ignore_unreachable: play.ignore_unreachable,
        connect: play.connect.clone(),
        ..PlayTask::empty()
    };
    // `gather_facts` is the play's own first task and nothing else: the reference's `setup`
    // module, run through the ordinary Python path, whose `ansible_facts` the driver writes as a
    // managed host's own words. Laid out here, as `steps[0]`, rather than run beside the play:
    // the pre-flight then reads it, the run builds it a payload with every other module the play
    // names, and its result comes back through the one road a result becomes a variable by.
    //
    // `always` is what the reference tags it, so `--tags deploy` still gathers and
    // `--skip-tags always` still does not. The play's own keywords come down through `empty` like
    // any other task's, except `run_once`: facts are the host's own, and gathering them once for
    // the batch would write one host's hostname onto every host of it.
    //
    // Named `ansible.legacy.setup`, as the reference's `gather_facts` plugin runs it: a
    // `library/setup.py` beside the playbook answers the gathering there, measured on
    // ansible-core 2.19.12, where `ansible.builtin.setup` would never find it.
    let gather = PlayTask {
        name: "Gathering Facts".to_string(),
        named: true,
        module: "ansible.legacy.setup".to_string(),
        tags: {
            let mut tags = empty.tags.clone();
            tags.push("always".to_string());
            tags.sort();
            tags.dedup();
            tags
        },
        run_once: None,
        args: gather_args(play),
        origin: play.origin.clone(),
        ..empty.clone()
    };
    let gathers = play.gather_facts && selection.selects(&gather.tags);
    if gathers {
        let origin = builder.origin();
        builder.steps.push(Step {
            kind: StepKind::Task,
            task: gather,
            block: None,
            section: Section::Body,
            role: None,
            origin,
            include_params: None,
            hosts: None,
        });
    }
    // A flush point closes each of the three sections. Measured on ansible-core 2.19.12: the
    // implicit ones are after `pre_tasks`, after the roles and `tasks` **together**, and after
    // `post_tasks`, and what they run counts `ok`/`changed` in the recap like any other task.
    //
    // Laid out here, as the walk reaches each boundary, rather than inserted afterwards: a step
    // put into the finished list would grow the span of a block that happens to end exactly
    // there, and a host draining that block's `always` after a failure would then walk into a
    // flush it has no business in. Whether the play has a handler at all is not known yet - a
    // role read below contributes its own - so all three are always there. A play with nothing
    // to flush pays three steps that report and show nothing.
    let mut mark = builder.steps.len();
    builder.items(&play.pre_tasks, &empty, None, Section::Body, None)?;
    builder.flush_point(mark);
    mark = builder.steps.len();
    let mut seen: Seen = Vec::new();
    for entry in &play.roles {
        builder.role(entry, &empty, &mut seen, None, Section::Body)?;
    }
    builder.items(&play.tasks, &empty, None, Section::Body, None)?;
    builder.flush_point(mark);
    mark = builder.steps.len();
    builder.items(&play.post_tasks, &empty, None, Section::Body, None)?;
    builder.flush_point(mark);
    // The play's own handlers run behind every role's, measured.
    for handler in &play.handlers {
        // The play's `module_defaults` and `collections` reach a handler as they reach a task.
        let mut task = handler.task.clone();
        task.module_defaults = empty
            .module_defaults
            .iter()
            .chain(&handler.task.module_defaults)
            .cloned()
            .collect();
        task.collections = handler
            .task
            .collections
            .clone()
            .or_else(|| empty.collections.clone());
        // And the play's check and diff modes, `--check` and `--diff` among them: a handler
        // that ran for real under `--check` would change the host.
        task.check_mode = task.check_mode.or_else(|| empty.check_mode.clone());
        task.diff = task.diff.or_else(|| empty.diff.clone());
        builder.handlers.push(Handler {
            task,
            listen: handler.listen.clone(),
            role: None,
        });
    }

    // Every role lends its `defaults` and its `vars` to the whole play, so the two layers are
    // folded once here rather than accumulated as the list is walked, and each role's own values
    // are laid back over them for its own steps. Measured both ways: a role sees a variable only
    // a later role sets, and a role whose own `vars` name it keeps its own value.
    let mut exported = RoleVars::default();
    for role in &builder.roles {
        extend(&mut exported.defaults, &role.defaults);
    }
    for role in &builder.roles {
        extend(&mut exported.vars, &role.vars);
    }
    let mut roles = builder.roles;
    for role in &mut roles {
        role.defaults = layered(&exported.defaults, &role.defaults);
        role.vars = layered(&exported.vars, &role.vars);
    }
    let mut steps = builder.steps;
    let mut handlers = builder.handlers;
    let warnings = crate::collection::resolve_all(&mut steps, &mut handlers, search);
    Ok(Compiled {
        steps,
        blocks: builder.blocks,
        roles,
        exported,
        handlers,
        inherited: empty,
        search: search.clone(),
        selection: selection.clone(),
        gathers,
        crosses: Vec::new(),
        resolved: Arc::default(),
        warnings,
        start_at: None,
    })
}

/// What one host asked for at an include step, for one item of its loop.
#[derive(Debug, Clone)]
pub(crate) struct IncludeRequest {
    pub target: IncludeTarget,
    /// What the `included:` line names: the absolute path of the file, or the role's name.
    pub what: String,
    /// The variables the statement hands to everything it brings in: its own `vars:`, the loop
    /// variable of the item that asked for it and `ansible_loop_var` beside it.
    pub vars: IncludeParams,
    /// The item's display label, which the `included:` line carries as `=> (item=...)`.
    pub label: Option<String>,
}

impl IncludeRequest {
    /// What two hosts have to agree on to be named on one `included:` line.
    ///
    /// Measured on ansible-core 2.19.12: two hosts including the same file share a line
    /// (`included: /abs/inc-a.yml for h1, h2`), two hosts whose rendered name differs get one
    /// line each, and a loop gets one line per item with its own `=> (item=)`. The variables are
    /// part of it because two hosts handing the same file different values are two different
    /// expansions, whatever the line would read.
    pub fn key(&self) -> String {
        serde_json::json!({
            "what": &self.what,
            "vars": sorted(&self.vars.values),
            "untrusted": &self.vars.untrusted,
            "label": &self.label,
        })
        .to_string()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum IncludeTarget {
    /// A tasks file, already resolved to an absolute path that exists.
    File(PathBuf),
    /// A role whose directory the driver has already found, so the message a role nobody can find
    /// gets is the one the reference gives it rather than the compiler's.
    Role(Box<RoleEntry>),
}

/// The steps, blocks, roles and handlers one include request brings in, compiled against the
/// play's own keywords and the run's tag selection.
///
/// The indices inside it are its own: a block it lays out is numbered from zero and a role it
/// reads is numbered from zero, because nothing here knows where the result will land. [`graft`]
/// is what rebases them onto the running play, and it is the only caller.
///
/// What comes down from the statement is measured, and it is less than the plan assumed: only its
/// `vars:`. The tags written on the statement stop there (`--tags inc` runs the include and not
/// the tasks it brought in), `when` is evaluated for the statement alone, and `become` and
/// `environment` are not even accepted on one - ansible-core 2.19.12 refuses them at load time
/// with `'environment' is not a valid attribute for a TaskInclude`. What every layer **around**
/// the statement says still reaches them ([`Origin::inherited`]), measured for tags: `--tags <a
/// play tag>`, `--tags <the role entry's tag>` and `--tags <a tag of the block around the
/// statement>` each run a task an `include_tasks` brought in.
///
/// **An `Err` from here is one host's task failure**, rescuable and `ignore_errors`-able, because
/// the single caller turns it into that. The pre-flight over the spliced content at the end relies
/// on it: a refusal has to read as the statement failing for this host, not as the run stopping.
/// A new caller that is not a run-time splice would break that and would also pull the pre-flight
/// into a path it must stay out of - listing and syntax-checking never refuse, so a playbook can
/// be read ahead of the release that runs it.
pub(crate) fn expand_include(
    base: &Compiled,
    parent: &Step,
    request: &IncludeRequest,
) -> anyhow::Result<Compiled> {
    let depth = parent.origin.depth + 1;
    // The layers around the statement, the play's among them, without the statement's own
    // keywords. What the statement hands down does not travel in the merge: it is a variable
    // layer of its own, laid on by `graft` at the precedence measured for it, and putting it here
    // as well would put it under the host's facts instead of over them.
    let inherited = parent
        .origin
        .inherited
        .as_deref()
        .unwrap_or(&base.inherited)
        .clone();
    let mut builder = Builder {
        steps: Vec::new(),
        blocks: Vec::new(),
        roles: Vec::new(),
        handlers: Vec::new(),
        search: &base.search,
        file_dir: parent.origin.file_dir.clone(),
        role_dir: parent.origin.role_dir.clone(),
        depth: 0,
        include_depth: depth,
        selection: &base.selection,
    };
    // `block` and `role` are seeded empty and rebased by `graft`: a sub-compilation that mixed
    // the running play's absolute indices with its own relative ones would be a list where the
    // reader cannot tell which it is holding, and a block index off by the wrong base is how a
    // host walks into a section it never entered.
    match &request.target {
        IncludeTarget::File(path) => {
            let tasks = crate::playbook::parse_tasks_file(path)?;
            builder.file_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            builder.items(&tasks, &inherited, None, parent.section, None)?;
        }
        IncludeTarget::Role(entry) => {
            // A fresh `seen` list, as for `import_role`: measured, an `include_role` runs its role
            // again even when the play's `roles:` list has already run it. Its own dependencies
            // still deduplicate against each other.
            builder.role(entry, &inherited, &mut Vec::new(), None, parent.section)?;
        }
    }
    let mut steps = builder.steps;
    let mut handlers = builder.handlers;
    // Warned about by the compilation of the play; an include read while it runs has no
    // place to say it.
    let _ = crate::collection::resolve_all(&mut steps, &mut handlers, &base.search);
    let expanded = Compiled {
        steps,
        blocks: builder.blocks,
        roles: builder.roles,
        exported: RoleVars::default(),
        handlers,
        inherited,
        search: base.search.clone(),
        selection: base.selection.clone(),
        // An expansion is spliced into a play that has already gathered, so it never carries a
        // gather step of its own.
        gathers: false,
        crosses: Vec::new(),
        resolved: Arc::clone(&base.resolved),
        warnings: Vec::new(),
        start_at: None,
    };
    // The pre-flight ran once, over the compilation, and none of this was in it. Refused here,
    // before anything is spliced, so what a file this release cannot execute gets is the
    // statement's own failure rather than a keyword read and then dropped.
    crate::preflight::check_spliced(&expanded)?;
    Ok(expanded)
}

/// Puts the expansions of one include step into the running play, at `at`, and answers with the
/// steps that went in.
///
/// Each group brings its own numbering, so three things are rebased: a step's `block` and `role`,
/// and a span's `parent` and its three ranges. `None` means "whatever the statement itself sat
/// in", which is what puts an included task in the block and the section the include was written
/// in - measured, an `include_tasks` in a block's body whose included task fails is taken by that
/// block's `rescue`.
///
/// The spans are appended **after** [`Compiled::splice`] has run, with their indices already
/// absolute: `splice` grows the ranges that were there before it, and a range it has never seen
/// must not be grown twice.
/// One expansion on its way into the running play.
pub(crate) struct Grafted {
    pub expanded: Compiled,
    /// What the statement handed down, for every step of this expansion.
    pub params: IncludeParams,
    /// The hosts whose own statement asked for it.
    pub hosts: Arc<[String]>,
}

pub(crate) fn graft(
    base: &mut Compiled,
    at: usize,
    parent: &Step,
    groups: Vec<Grafted>,
) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut spans: Vec<BlockSpan> = Vec::new();
    for Grafted {
        expanded: sub,
        params,
        hosts,
    } in groups
    {
        let block_offset = base.blocks.len() + spans.len();
        let role_offset = base.roles.len();
        let step_offset = at + steps.len();
        let params = Arc::new(params);
        for mut step in sub.steps {
            step.block = step.block.map(|b| b + block_offset).or(parent.block);
            step.role = step.role.map(|r| r + role_offset).or(parent.role);
            step.include_params = Some(Arc::clone(&params));
            step.hosts = Some(Arc::clone(&hosts));
            steps.push(step);
        }
        for mut span in sub.blocks {
            for range in [&mut span.body, &mut span.rescue, &mut span.always] {
                range.start += step_offset;
                range.end += step_offset;
            }
            span.parent = span.parent.map(|p| p + block_offset).or(parent.block);
            spans.push(span);
        }
        base.roles.extend(sub.roles);
        // Behind the play's own, measured: a play handler and an included role's handler notified
        // in that order run in that order. Appending is also what keeps every index a host is
        // already carrying in its notification list pointing at the handler it named.
        base.handlers.extend(sub.handlers);
    }
    base.splice(at, steps.clone());
    base.blocks.extend(spans);
    steps
}

/// The role one `include_role` task names, read out of its arguments and whatever the item
/// rendered for them.
pub(crate) fn include_role_entry(args: &Map<String, Value>) -> anyhow::Result<RoleEntry> {
    include_role_options(args)?;
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let name = text("name")
        .or_else(|| text("role"))
        .ok_or_else(|| anyhow::anyhow!("'include_role' takes a role name"))?;
    let from = |key: &str, fallback: &str| text(key).unwrap_or(fallback).to_string();
    Ok(RoleEntry {
        name: name.to_string(),
        from: crate::roles::RoleFrom {
            tasks: from("tasks_from", "main"),
            vars: from("vars_from", "main"),
            defaults: from("defaults_from", "main"),
            handlers: from("handlers_from", "main"),
        },
        // As for `import_role`: everything the statement says is an argument of the statement,
        // and what it hands the role travels in the task's own `vars:`.
        params: Map::new(),
        keywords: PlayTask::empty(),
    })
}

/// The arguments `include_role` takes, and the three it takes that this release cannot honour.
///
/// `public: true` is measured to export the role's `defaults` and `vars` to everything behind the
/// include, where the default `public: false` keeps them to the role's own tasks - so honouring it
/// would mean changing the play's variable layers while hosts stand at different steps of it.
/// Refused by name, the way `import_role` refuses the same three, rather than accepted and
/// dropped - but by value for `public`, whose false is what this release does. See
/// `refuse_public`.
fn include_role_options(args: &Map<String, Value>) -> anyhow::Result<()> {
    unknown_options(
        "include_role",
        args,
        &[
            "name",
            "role",
            "tasks_from",
            "vars_from",
            "defaults_from",
            "handlers_from",
            "public",
            "allow_duplicates",
            "rolespec_validate",
            "apply",
        ],
        &["allow_duplicates", "rolespec_validate", "apply"],
    )?;
    refuse_public("include_role", args)
}

/// `public` is the one refusable argument whose **false** is what this release already does.
///
/// The others name work that is missing: nothing dedups a role, the argument-spec check has no
/// off switch, and there is no layer to apply keywords through, so writing any of them at all is
/// asking for something that will not happen. `public: false` asks for the role's `defaults` and
/// `vars` to stay behind the include, which is exactly what happens here - refusing it by
/// presence turns a statement that asks for the engine's own behaviour into an error.
///
/// Anything that is not a spelled-out false is refused, a template included: what `public` will
/// be worth is not known where this runs, and the safe answer for an export this release cannot
/// do is to say so rather than to guess.
fn refuse_public(statement: &str, args: &Map<String, Value>) -> anyhow::Result<()> {
    match args.get("public") {
        None => Ok(()),
        Some(value) if crate::executor::as_bool_value(value) == Some(false) => Ok(()),
        Some(_) => bail!("'{statement}' option 'public' is not supported yet"),
    }
}

/// `include_tasks` takes a file name and nothing else. `apply:` puts keywords on everything the
/// statement brings in, which this release has no layer for, so it is refused by its own name.
fn include_options(statement: &str, args: &Map<String, Value>) -> anyhow::Result<()> {
    unknown_options(
        statement,
        args,
        &["file", "_raw_params", "apply"],
        &["apply"],
    )
}

/// Everything an `include_vars` can be told, and the seven it can be told that this release
/// cannot honour.
///
/// `dir:` is measured to work in the reference, reading every file of a directory in name order,
/// and the other six narrow which of those files are read or how their values are merged. Refused
/// by their own names rather than read and dropped: a run that silently loaded none of a
/// directory's variables would go on to apply a playbook against defaults nobody wrote.
///
/// Checked here, where the statement becomes a step, rather than in the module: that puts the
/// refusal before the first connection for a playbook's own `include_vars` and inside the failing
/// host's own task for one an include brought in, which is where each belongs.
pub(crate) fn include_vars_options(args: &Map<String, Value>) -> anyhow::Result<()> {
    const SEARCHES: &[&str] = &[
        "dir",
        "depth",
        "extensions",
        "files_matching",
        "ignore_files",
        "ignore_unknown_extensions",
        "hash_behaviour",
    ];
    let mut known = vec!["_raw_params", "file", "name"];
    known.extend_from_slice(SEARCHES);
    unknown_options("include_vars", args, &known, SEARCHES)
}

fn extend(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
}

fn layered(under: &Map<String, Value>, over: &Map<String, Value>) -> Map<String, Value> {
    let mut out = under.clone();
    extend(&mut out, over);
    out
}

/// One task with the keywords of everything above it folded in. The inner value wins wherever it
/// says anything, and `when` concatenates outer first: measured on ansible-core 2.19.12, a block
/// with a false `when` and a task with a true one skips reporting the **block's** condition as
/// the `false_condition`, so the outer condition has to be evaluated first.
fn merge(outer: &PlayTask, inner: &PlayTask) -> PlayTask {
    let mut task = inner.clone();
    task.when = outer
        .when
        .iter()
        .chain(&inner.when)
        .cloned()
        .collect::<Vec<_>>();
    // Measured: a block saying `ignore_errors: true` loses to a task saying `ignore_errors:
    // false` inside it, which is why the keyword is three-state all the way down here.
    task.ignore_errors = inner
        .ignore_errors
        .clone()
        .or_else(|| outer.ignore_errors.clone());
    task.timeout = inner.timeout.or(outer.timeout);
    task.throttle = inner.throttle.or(outer.throttle);
    task.debugger = inner.debugger.or(outer.debugger);
    // All three-state for the same reason `ignore_errors` is: a play or a block that says one
    // thing and a task that says the other have to be told apart from a task that says nothing.
    task.no_log = inner.no_log.or(outer.no_log);
    task.check_mode = inner
        .check_mode
        .clone()
        .or_else(|| outer.check_mode.clone());
    task.diff = inner.diff.clone().or_else(|| outer.diff.clone());
    // Three-state for the same reason, and inherited the same way: `Block.fattributes` carries
    // all three, `Play.fattributes` carries `run_once` alone, and the innermost value wins.
    task.run_once = inner.run_once.or(outer.run_once);
    task.any_errors_fatal = inner.any_errors_fatal.or(outer.any_errors_fatal);
    task.ignore_unreachable = inner.ignore_unreachable.or(outer.ignore_unreachable);
    task.delegate_facts = inner.delegate_facts.or(outer.delegate_facts);
    task.delegate_to = inner
        .delegate_to
        .clone()
        .or_else(|| outer.delegate_to.clone());
    // `environment` is the one keyword here that does not have a winner: measured on
    // ansible-core 2.19.12, a play's layer and a task's layer are both in force and the task's
    // wins name by name, so the layers are stacked outermost first and merged when the task runs.
    task.environment = outer
        .environment
        .iter()
        .chain(&inner.environment)
        .cloned()
        .collect();
    // `module_defaults` stacks layers like `environment`; the winner per module is settled when
    // the task runs, where the values are rendered.
    task.module_defaults = outer
        .module_defaults
        .iter()
        .chain(&inner.module_defaults)
        .cloned()
        .collect();
    task.collections = inner
        .collections
        .clone()
        .or_else(|| outer.collections.clone());
    task.connect = inner.connect.over(&outer.connect);
    task.r#become = inner.r#become.or(outer.r#become);
    task.become_user = inner
        .become_user
        .clone()
        .or_else(|| outer.become_user.clone());
    // Tags are a set, and they only ever grow downwards: measured on ansible-core 2.19.12, a
    // task inside a block tagged `outer` is selected by `--skip-tags outer` as surely as the
    // block is, and it lists as `TAGS: [inner, outer]` with its own. Sorted and deduplicated
    // here because every reader of the list treats it as a set - the listings sort what they
    // print, and the selection asks whether a name is in it.
    task.tags = inner
        .tags
        .iter()
        .chain(&outer.tags)
        .cloned()
        .collect::<Vec<_>>();
    task.tags.sort_unstable();
    task.tags.dedup();
    // A `notify` written on a block reaches the tasks under it - measured on ansible-core
    // 2.19.12, a block carrying `notify: h` runs `h` when a task inside it changes something -
    // and a task that notifies something of its own keeps both. Left in the order they were
    // written, with the task's own first: nothing depends on it, since handlers run in
    // definition order, but a refusal names the first name the playbook wrote.
    for name in &outer.notify {
        if !task.notify.contains(name) {
            task.notify.push(name.clone());
        }
    }
    // The outer map first, so a name the task sets itself keeps the task's value.
    let mut vars = outer.vars.clone();
    vars.extend(inner.vars.clone());
    task.vars = vars;
    // A keyword this release cannot execute has to be refused wherever it was written, and once
    // a role's tasks come from another file the only place the pre-flight still sees them is
    // here. Carrying the outer keyword down means a `tags:` on a role entry, or a `notify:` on a
    // block inside a role, is refused by its own name rather than inherited and ignored.
    for kw in &outer.unsupported {
        if !task.unsupported.contains(kw) {
            task.unsupported.push(kw);
        }
    }
    task.unsupported.sort_unstable();
    task
}

/// The blocks around a step, innermost first, each with the section of that block the step sits
/// in. One walk, so the three functions below decide different things about the same tree
/// rather than each re-deriving it from the indices.
fn ancestors(c: &Compiled, index: usize) -> impl Iterator<Item = (usize, Section)> + '_ {
    let step = &c.steps[index];
    let mut next = step.block.map(|id| (id, step.section));
    std::iter::from_fn(move || {
        let (id, section) = next?;
        let span = &c.blocks[id];
        next = span.parent.map(|parent| (parent, span.section));
        Some((id, section))
    })
}

/// The rescues a host standing on `index` is already inside, innermost first: the sections it
/// may walk through that a host on the happy path steps over.
///
/// Read off the step rather than carried by the host because a step names every block around it
/// and the section it sits in each of them, so the answer is the same whichever way the host
/// arrived. It is the set every seek from that step needs: a cleanup or a recovery written
/// inside a rescue is only reachable for a host that entered that rescue.
fn entered(c: &Compiled, index: usize) -> Vec<usize> {
    ancestors(c, index)
        .filter(|&(_, section)| section == Section::Rescue)
        .map(|(id, _)| id)
        .collect()
}

/// The first index at or after `from` that a host has a reason to run, given the rescues it is
/// already inside, or the length of the list when there is none.
///
/// A step is stepped over exactly when it sits in a `rescue` the host has not entered: rescue is
/// the one section reached only by failing, and a host already working through one carries on
/// to its end. Everything else - a body, an `always`, a block written inside either - is on the
/// way. Nothing here can name a step that does not exist.
fn seek(c: &Compiled, from: usize, entered: &[usize]) -> usize {
    (from..c.steps.len())
        .find(|&i| {
            !ancestors(c, i)
                .any(|(id, section)| section == Section::Rescue && !entered.contains(&id))
        })
        .unwrap_or(c.steps.len())
}

/// Whether a step sits in the `block:` list of every block around it.
///
/// Measured on ansible-core 2.19.12: `--list-tasks` and `--list-tags` walk a block's body and
/// nothing else, so a task written in a `rescue:` or an `always:` is listed nowhere and its tags
/// do not reach `TASK TAGS`. That is the reference's own behaviour, not an omission of ours -
/// which is why the listing asks this rather than filtering by hand.
///
/// A flush point the compiler put in itself is listed nowhere either, and neither is a handler:
/// measured, `--list-tasks` on a playbook full of handlers shows the `meta` a playbook wrote and
/// not one handler. The step is engine structure rather than something the playbook says.
pub(crate) fn listed(c: &Compiled, index: usize) -> bool {
    // The implicit `Gathering Facts` step is run and reported like any other task and listed like
    // none of them: measured, the reference shows no such line in `--list-tasks` and no `always`
    // in `TASK TAGS`, for a play whose `gather_facts` is on by default.
    if c.gathers && index == 0 {
        return false;
    }
    match c.steps[index].kind {
        StepKind::Flush { explicit: false } | StepKind::Handler(_) => return false,
        _ => {}
    }
    ancestors(c, index).all(|(_, section)| section == Section::Body)
}

/// The step a host starts the play on. Not always the first one: `block: []` with a `rescue`
/// lays that rescue out at index 0, and nothing has failed yet.
pub(crate) fn first(c: &Compiled) -> usize {
    match c.start_at {
        Some(at) if c.gathers && at < c.steps.len() => 0,
        Some(at) => seek(c, at, &[]),
        None => seek(c, 0, &[]),
    }
}

/// The first step `--start-at-task` names, as the reference finds it: a task (or the statement of
/// an include) whose name matches the pattern, `Gathering Facts` and handlers apart.
pub(crate) fn find_task(c: &Compiled, pattern: &str) -> Option<usize> {
    (usize::from(c.gathers)..c.steps.len()).find(|&i| {
        matches!(c.steps[i].kind, StepKind::Task | StepKind::Include(_))
            && crate::inventory::glob_match(pattern, &c.steps[i].task.name)
    })
}

/// The index a host moves to after finishing `pos` without failing: the next step it has a
/// reason to run, past the rescue of every block it leaves and of every block it enters. A
/// block whose `block:` list is empty is entered *on* its rescue, so entry needs the same care
/// as exit.
pub(crate) fn after(c: &Compiled, pos: usize) -> usize {
    // The `Gathering Facts` step is followed by the step `--start-at-task` named.
    if let Some(at) = c.start_at
        && c.gathers
        && pos == 0
        && at < c.steps.len()
    {
        return seek(c, at, &[]);
    }
    seek(c, pos + 1, &entered(c, pos))
}

/// Where a host goes when the task at `pos` has just failed and a `rescue` takes that failure:
/// the first step of that rescue, or `None` when nothing rescues it and the host is on its way
/// out of the play.
///
/// The rescue that takes a failure belongs to the innermost enclosing block whose **body** holds
/// the step and whose rescue section has steps in it. A failure raised inside a rescue, or
/// inside an `always`, belongs to whatever encloses that block and never to the block itself -
/// a rescue does not rescue itself, which would be a loop. Measured on ansible-core 2.19.12:
/// a rescue that fails leaves the play through the `always` sections around it (`failed=1
/// rescued=1`), the same failure is taken by a **grandparent's** rescue when there is one
/// (`rescued=2`, exit 0), and a failure raised in a child block's `always` is taken by the
/// parent's rescue with the rest of that `always` left unrun (`rescued=1`, exit 0).
pub(crate) fn rescue_target(c: &Compiled, pos: usize) -> Option<usize> {
    let (id, _) = ancestors(c, pos)
        .find(|&(id, section)| section == Section::Body && !c.blocks[id].rescue.is_empty())?;
    // Entered like any other step: a rescue that opens on a block with an empty `block:` list
    // opens on that block's own rescue, which this failure has not entered. The rescues the
    // host was already inside travel with it, or the recovery of a block written inside one of
    // them would be stepped over on the way in.
    let mut inside = entered(c, pos);
    inside.push(id);
    // No `start < end` guard here, unlike `after_failure` below: measured and deliberate. A
    // rescue whose only step sits inside a nested block's own un-entered rescue - `block: []`
    // there, with a `rescue:` of its own - has nothing to run, and the reference still counts
    // `rescued=1` and exits 0, walking straight into whatever comes after. Guarding this the
    // way `after_failure` does would turn that into an unrescued failure instead.
    Some(seek(c, c.blocks[id].rescue.start, &inside))
}

/// Where a host goes when the task at `pos` has just failed: the `always` section of the
/// innermost block that still has one to run, as the pair (its first step, the index it ends
/// at), or `None` when the play is over for this host.
///
/// Measured on ansible-core 2.19.12: a task failing inside a nested block runs the inner
/// `always`, then the outer `always`, then leaves the play - the step after the outer block
/// never runs and the recap counts the two `always` tasks as `ok`. A task failing **inside** an
/// `always` does not finish that section: the rest of it is skipped and the host leaves, and
/// that holds for a task inside a block written inside the `always` too - measured, a cleanup
/// of two tasks in a nested block whose first task fails runs neither the second nor the step
/// behind the nested block, and the recap reads `failed=2`.
pub(crate) fn after_failure(c: &Compiled, pos: usize) -> Option<(usize, usize)> {
    let inside = entered(c, pos);
    ancestors(c, pos)
        .filter(|&(_, section)| section != Section::Always)
        .map(|(id, _)| {
            let always = &c.blocks[id].always;
            // Entered like any other step, so a cleanup that opens on a block with an empty
            // `block:` list opens on that block's rescue and is not run there either. A
            // section with nothing left to run is no section at all, and the walk goes on.
            //
            // The rescues the host is inside travel with it: a block written in a rescue has
            // its own `always`, and measured on ansible-core 2.19.12 that cleanup runs when
            // the block fails. Seeking with an empty set would step over every one of its
            // steps - the whole cleanup skipped, and nothing said about it.
            (seek(c, always.start, &inside), always.end)
        })
        .find(|&(start, end)| start < end)
}

/// The same, for a host already draining the `always` section that ends at `cleanup`: on
/// through that section, or out into the next one.
///
/// The whole section runs, blocks written inside it included. Which section that is cannot be
/// read off the step: a cleanup task inside a nested block carries its own block's `Body`, and
/// a cleanup that ends where a nested `always` ends is not the end of the section holding it.
/// The host knows which section it entered, so it carries the end of it and this compares
/// against that. Measured on ansible-core 2.19.12: a cleanup made of a nested block of two
/// tasks runs both, and a nested block with an `always` of its own runs that too and then the
/// step behind it, `ok=3 failed=1`.
pub(crate) fn after_pending(c: &Compiled, pos: usize, cleanup: usize) -> Option<(usize, usize)> {
    let next = after(c, pos);
    if next < cleanup {
        return Some((next, cleanup));
    }
    after_failure(c, pos)
}

#[cfg(test)]
mod tests;
