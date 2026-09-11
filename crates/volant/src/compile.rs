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

use std::ops::Range;

use crate::playbook::{Block, Play, PlayTask, TaskOrBlock};

/// What one step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepKind {
    /// A module to run on the host.
    Task,
    /// `meta`: something asked of the engine rather than of the host. It shows a banner and
    /// reports nothing, which is why it is the one step kind that prints once per live host.
    Meta,
}

/// Which of a block's three lists a step came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Body,
    Rescue,
    Always,
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
}

/// Every `meta` action ansible-core 2.19.12 accepts, and whether this release honours it.
///
/// `noop` and `flush_handlers` do nothing here and nothing there: `noop` is defined as doing
/// nothing, and no handler can reach a run yet because `handlers`, `notify` and `force_handlers`
/// are all refused before it starts, so there is never anything to flush. The rest ask for
/// something this release cannot do and are refused by their own names.
pub(crate) const META_ACTIONS: &[(&str, bool)] = &[
    ("clear_facts", false),
    ("clear_host_errors", false),
    ("end_host", false),
    ("end_play", false),
    ("flush_handlers", true),
    ("noop", true),
    ("refresh_inventory", false),
    ("reset_connection", false),
];

/// The action a `meta` task asked for.
pub(crate) fn meta_action(task: &PlayTask) -> &str {
    task.args
        .get("_raw_params")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

struct Builder {
    steps: Vec<Step>,
    blocks: Vec<BlockSpan>,
}

impl Builder {
    fn items(
        &mut self,
        items: &[TaskOrBlock],
        inherited: &PlayTask,
        block: Option<usize>,
        section: Section,
    ) {
        for item in items {
            match item {
                TaskOrBlock::Task(task) => {
                    let task = merge(inherited, task);
                    let kind = if crate::playbook::is_meta(&task) {
                        StepKind::Meta
                    } else {
                        StepKind::Task
                    };
                    self.steps.push(Step {
                        kind,
                        task,
                        block,
                        section,
                    });
                }
                TaskOrBlock::Block(inner) => self.block(inner, inherited, block, section),
            }
        }
    }

    fn block(&mut self, b: &Block, inherited: &PlayTask, parent: Option<usize>, section: Section) {
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
        self.items(&b.body, &merged, Some(id), Section::Body);
        let rescue = self.steps.len();
        self.items(&b.rescue, &merged, Some(id), Section::Rescue);
        let always = self.steps.len();
        self.items(&b.always, &merged, Some(id), Section::Always);
        let end = self.steps.len();
        self.blocks[id].body = start..rescue;
        self.blocks[id].rescue = rescue..always;
        self.blocks[id].always = always..end;
    }
}

/// A play's task list, flattened.
///
/// The play's own `become` and `become_user` are deliberately left out of the merge: their
/// precedence against a host variable was measured in an earlier release and lives in the
/// executor, which reads them from the play. A block is a task keyword, so its `become` does
/// come down here.
pub(crate) fn compile(play: &Play) -> Compiled {
    let mut builder = Builder {
        steps: Vec::new(),
        blocks: Vec::new(),
    };
    builder.items(&play.tasks, &PlayTask::empty(), None, Section::Body);
    Compiled {
        steps: builder.steps,
        blocks: builder.blocks,
    }
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
    task.ignore_errors = inner.ignore_errors.or(outer.ignore_errors);
    task.timeout = inner.timeout.or(outer.timeout);
    task.r#become = inner.r#become.or(outer.r#become);
    task.become_user = inner
        .become_user
        .clone()
        .or_else(|| outer.become_user.clone());
    // The outer map first, so a name the task sets itself keeps the task's value.
    let mut vars = outer.vars.clone();
    vars.extend(inner.vars.clone());
    task.vars = vars;
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

/// The step a host starts the play on. Not always the first one: `block: []` with a `rescue`
/// lays that rescue out at index 0, and nothing has failed yet.
pub(crate) fn first(c: &Compiled) -> usize {
    seek(c, 0, &[])
}

/// The index a host moves to after finishing `pos` without failing: the next step it has a
/// reason to run, past the rescue of every block it leaves and of every block it enters. A
/// block whose `block:` list is empty is entered *on* its rescue, so entry needs the same care
/// as exit.
pub(crate) fn after(c: &Compiled, pos: usize) -> usize {
    let entered: Vec<usize> = ancestors(c, pos)
        .filter(|&(_, section)| section == Section::Rescue)
        .map(|(id, _)| id)
        .collect();
    seek(c, pos + 1, &entered)
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
    ancestors(c, pos)
        .filter(|&(_, section)| section != Section::Always)
        .map(|(id, _)| {
            let always = &c.blocks[id].always;
            // Entered like any other step, so a cleanup that opens on a block with an empty
            // `block:` list opens on that block's rescue and is not run there either. A
            // section with nothing left to run is no section at all, and the walk goes on.
            (seek(c, always.start, &[]), always.end)
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
mod tests {
    use super::*;
    use crate::playbook::parse;

    fn compiled(text: &str) -> Compiled {
        let pb = parse(text, "x.yml").unwrap_or_else(|e| panic!("{e:#}"));
        compile(&pb.plays[0])
    }

    fn names(c: &Compiled) -> Vec<&str> {
        c.steps.iter().map(|s| s.task.name.as_str()).collect()
    }

    /// The order a host walks a compiled play from a given step, on the happy path.
    fn walk(c: &Compiled, from: usize) -> Vec<&str> {
        let mut pos = from;
        let mut seen = Vec::new();
        while pos < c.steps.len() {
            seen.push(c.steps[pos].task.name.as_str());
            pos = after(c, pos);
        }
        seen
    }

    /// The order a host walks the cleanup after the step at `failed` has failed.
    fn walk_failure(c: &Compiled, failed: usize) -> Vec<&str> {
        let mut seen = Vec::new();
        let mut at = after_failure(c, failed);
        while let Some((pos, cleanup)) = at {
            seen.push(c.steps[pos].task.name.as_str());
            at = after_pending(c, pos, cleanup);
        }
        seen
    }

    const NESTED: &str = r#"
- hosts: all
  tasks:
    - name: before
      command: "true"
    - block:
        - name: body one
          command: "true"
        - block:
            - name: nested body
              command: "true"
          rescue:
            - name: nested rescue
              command: "true"
          always:
            - name: nested always
              command: "true"
      rescue:
        - name: outer rescue
          command: "true"
      always:
        - name: outer always
          command: "true"
    - name: after
      command: "true"
"#;

    /// The tree flattens in playbook order, section by section, and every block's span covers
    /// exactly the steps it holds.
    ///
    /// What would make this red: a section laid out in the wrong order, or a span whose end
    /// falls short of the steps it holds - both of which leave a step the driver walks past.
    #[test]
    fn a_nested_play_flattens_in_playbook_order() {
        let c = compiled(NESTED);
        assert_eq!(
            names(&c),
            [
                "before",
                "body one",
                "nested body",
                "nested rescue",
                "nested always",
                "outer rescue",
                "outer always",
                "after",
            ]
        );
        // The outer block is reserved first, so it is block 0 and the nested one is block 1.
        let outer = &c.blocks[0];
        let nested = &c.blocks[1];
        assert_eq!((outer.body.start, outer.body.end), (1, 5));
        assert_eq!((outer.rescue.start, outer.rescue.end), (5, 6));
        assert_eq!((outer.always.start, outer.always.end), (6, 7));
        assert_eq!(outer.parent, None);
        assert_eq!((nested.body.start, nested.body.end), (2, 3));
        assert_eq!((nested.rescue.start, nested.rescue.end), (3, 4));
        assert_eq!((nested.always.start, nested.always.end), (4, 5));
        assert_eq!(nested.parent, Some(0));
        assert_eq!(nested.section, Section::Body);
        // Every span ends inside the list it describes, which is what keeps `after` from
        // naming a step that does not exist.
        for span in &c.blocks {
            assert!(span.always.end <= c.steps.len());
            assert_eq!(span.body.end, span.rescue.start);
            assert_eq!(span.rescue.end, span.always.start);
        }
    }

    /// A host that fails nothing never enters a rescue, at any depth, and leaves each block
    /// through its `always`.
    ///
    /// What would make this red: `after` returning `pos + 1` everywhere, which walks both
    /// rescue sections on the happy path - the failure handling of a playbook running as if it
    /// were ordinary work.
    #[test]
    fn the_happy_path_steps_over_every_rescue() {
        let c = compiled(NESTED);
        assert_eq!(
            walk(&c, 0),
            [
                "before",
                "body one",
                "nested body",
                "nested always",
                "outer always",
                "after",
            ]
        );
    }

    /// A block that ends where its parent's body ends leaves through the parent's `always`, not
    /// through the parent's `rescue`.
    ///
    /// What would make this red: `after` returning the inner block's `always.start` and stopping
    /// there. That index is the parent's rescue when the inner block has no `always` of its own,
    /// so the happy path would run the outer recovery with nothing to recover from.
    #[test]
    fn leaving_a_block_at_the_end_of_a_parent_body_steps_over_the_parent_rescue() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - block:
            - name: inner only
              command: "true"
      rescue:
        - name: outer rescue
          command: "true"
      always:
        - name: outer always
          command: "true"
    - name: after
      command: "true"
"#,
        );
        assert_eq!(
            names(&c),
            ["inner only", "outer rescue", "outer always", "after"]
        );
        assert_eq!(walk(&c, 0), ["inner only", "outer always", "after"]);
    }

    /// A block with neither section behaves like the tasks written out in its place, and a task
    /// outside every block just moves on.
    #[test]
    fn a_bare_block_and_a_bare_task_move_to_the_next_step() {
        let c = compiled(
            "- hosts: all\n  tasks:\n    - block:\n        - name: one\n          command: \"true\"\n    - name: two\n      command: \"true\"\n",
        );
        assert_eq!(walk(&c, 0), ["one", "two"]);
    }

    /// A rescue that is empty leaves `always` starting where the body ended, so the happy path
    /// walks straight into it.
    #[test]
    fn an_empty_rescue_leads_straight_into_always() {
        let c = compiled(
            "- hosts: all\n  tasks:\n    - block:\n        - name: one\n          command: \"true\"\n      always:\n        - name: cleanup\n          command: \"true\"\n",
        );
        assert_eq!(c.blocks[0].rescue, 1..1);
        assert_eq!(walk(&c, 0), ["one", "cleanup"]);
    }

    /// A failure runs the `always` of every block around it, innermost first, and then the play
    /// is over for that host.
    ///
    /// What would make this red: a failure that leaves the play straight away, which skips the
    /// cleanup the playbook wrote; or one that walks into the outer block's body again.
    #[test]
    fn a_failure_runs_every_enclosing_always_and_then_stops() {
        let c = compiled(NESTED);
        // "nested body" fails.
        assert_eq!(walk_failure(&c, 2), ["nested always", "outer always"]);
    }

    /// A failure inside an `always` section does not finish it: measured, the rest of the
    /// section is skipped and the host leaves through whatever is outside.
    #[test]
    fn a_failure_inside_an_always_leaves_the_rest_of_it() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: body
          command: "true"
      always:
        - name: first cleanup
          command: "true"
        - name: second cleanup
          command: "true"
"#,
        );
        let cleanup = c.blocks[0].always.clone();
        assert_eq!(c.steps[cleanup.start].task.name, "first cleanup");
        assert_eq!(
            after_pending(&c, cleanup.start, cleanup.end)
                .map(|(p, _)| c.steps[p].task.name.as_str()),
            Some("second cleanup"),
            "a pending failure still finishes the section"
        );
        assert_eq!(
            after_failure(&c, cleanup.start),
            None,
            "a failure raised here ends the section instead"
        );
    }

    /// A cleanup written as a block runs to the end of the section, and a failure inside that
    /// block ends the cleanup where the reference ends it.
    ///
    /// Measured on ansible-core 2.19.12, on this shape: both cleanup tasks run and the recap
    /// reads `ok=2 changed=2 failed=1`; with the first of them failing instead, neither the
    /// second nor the step behind the nested block runs and the recap reads `failed=2`.
    ///
    /// What would make this red: reading the section a host is draining off the step it is on.
    /// A cleanup task inside a nested block carries that block's `Body`, so the host would take
    /// the first such step for the end of the cleanup and leave the play with the rest of it
    /// never run and never reported - a run that exits having done less than the playbook says.
    #[test]
    fn a_cleanup_written_as_a_block_runs_to_the_end_of_the_section() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - block:
            - name: cleanup one
              command: "true"
            - name: cleanup two
              command: "true"
        - name: cleanup last
          command: "true"
"#,
        );
        assert_eq!(
            walk_failure(&c, 0),
            ["cleanup one", "cleanup two", "cleanup last"]
        );
        assert_eq!(
            after_failure(&c, 1),
            None,
            "a failure inside the nested cleanup ends the section, measured"
        );
    }

    /// A nested cleanup that has an `always` of its own runs it, and the section holding it
    /// still finishes.
    ///
    /// Measured on ansible-core 2.19.12 on this shape: `ok=3 changed=3 failed=1`.
    #[test]
    fn a_cleanup_block_runs_its_own_always_and_the_section_goes_on() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - block:
            - name: cleanup one
              command: "true"
          always:
            - name: cleanup inner cleanup
              command: "true"
        - name: cleanup last
          command: "true"
"#,
        );
        assert_eq!(
            walk_failure(&c, 0),
            ["cleanup one", "cleanup inner cleanup", "cleanup last"]
        );
    }

    /// The same on the failure path: a cleanup that opens on a block with an empty `block:` list
    /// opens on that block's rescue, and a host walking out through the `always` sections has
    /// failed somewhere else entirely.
    ///
    /// What would make this red: entering an `always` at its first index rather than at its
    /// first step, which runs a recovery section as cleanup.
    #[test]
    fn a_cleanup_does_not_start_in_a_rescue_either() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block:
        - name: fails
          command: "true"
      always:
        - block: []
          rescue:
            - name: cleanup recovery
              command: "true"
        - name: cleanup
          command: "true"
"#,
        );
        assert_eq!(names(&c), ["fails", "cleanup recovery", "cleanup"]);
        assert_eq!(walk_failure(&c, 0), ["cleanup"]);
    }

    /// A block with an empty `block:` list is entered on its rescue, and nothing has failed:
    /// the happy path steps over it, whether the block is the first thing in the play or comes
    /// after another step.
    ///
    /// What would make this red: a walk that only handles leaving a block. Entering one whose
    /// body is empty lands straight on the first step of its rescue, so the recovery path of a
    /// playbook would run as ordinary work.
    #[test]
    fn an_empty_body_is_not_a_way_into_a_rescue() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - block: []
      rescue:
        - name: first recovery
          command: "true"
    - name: one
      command: "true"
    - block: []
      rescue:
        - name: second recovery
          command: "true"
      always:
        - name: cleanup
          command: "true"
    - name: two
      command: "true"
"#,
        );
        assert_eq!(
            names(&c),
            ["first recovery", "one", "second recovery", "cleanup", "two"]
        );
        assert_eq!(first(&c), 1, "the play does not start inside a rescue");
        assert_eq!(walk(&c, first(&c)), ["one", "cleanup", "two"]);
    }

    /// Every keyword a block carries reaches the tasks under it, and a task that says something
    /// itself keeps its own answer.
    ///
    /// What would make this red: an inherited keyword dropped in the merge, which runs the task
    /// without the `become`, the `when` or the timeout the operator wrote around it.
    #[test]
    fn a_block_s_keywords_reach_the_tasks_under_it() {
        let c = compiled(
            r#"
- hosts: all
  tasks:
    - name: outer
      block:
        - name: inherits
          command: "true"
        - name: overrides
          command: "true"
          ignore_errors: false
          become: false
          vars: {shared: task}
          when: inner
      when: outer
      become: true
      become_user: deploy
      ignore_errors: true
      timeout: 7
      vars: {shared: block, only: block}
"#,
        );
        let inherits = &c.steps[0].task;
        assert_eq!(inherits.when, ["outer"]);
        assert_eq!(inherits.r#become, Some(true));
        assert_eq!(inherits.become_user.as_deref(), Some("deploy"));
        assert!(inherits.ignores_errors());
        assert_eq!(inherits.timeout, Some(7));
        assert_eq!(inherits.vars["shared"], serde_json::json!("block"));
        assert_eq!(inherits.vars["only"], serde_json::json!("block"));

        let overrides = &c.steps[1].task;
        assert_eq!(
            overrides.when,
            ["outer", "inner"],
            "the outer condition is evaluated first, so it is the one a skip reports"
        );
        assert_eq!(overrides.r#become, Some(false));
        assert!(
            !overrides.ignores_errors(),
            "measured: a task's own 'ignore_errors: false' beats the block's 'true'"
        );
        assert_eq!(overrides.vars["shared"], serde_json::json!("task"));
        assert_eq!(overrides.vars["only"], serde_json::json!("block"));
        // The block's name belongs to no step: measured, it is shown nowhere.
        assert_eq!(names(&c), ["inherits", "overrides"]);
    }

    /// A `meta` task compiles into a step of its own, so nothing tries to send it to a host.
    #[test]
    fn meta_compiles_into_a_step_of_its_own() {
        let c = compiled("- hosts: all\n  tasks:\n    - meta: noop\n    - command: \"true\"\n");
        assert_eq!(c.steps[0].kind, StepKind::Meta);
        assert_eq!(meta_action(&c.steps[0].task), "noop");
        assert_eq!(c.steps[1].kind, StepKind::Task);
    }

    /// The action table is the reference's own list, sorted and free of duplicates.
    #[test]
    fn the_meta_actions_are_sorted_and_free_of_duplicates() {
        let names: Vec<&str> = META_ACTIONS.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted);
    }
}
