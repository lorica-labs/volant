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
//! [`after`] never invents an index - it returns either the next step or the start of a section
//! that exists - and the driver tells the coordinator about every index it steps over, so a step
//! left out is a step the recap can still account for.

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

/// The index a host moves to after finishing `pos` without failing: the next step, or past a
/// rescue section it has no reason to enter, or out of however many blocks end here.
///
/// Nothing in here can name a step that does not exist: every index returned is either `pos + 1`
/// or the start of a section this compilation laid out.
pub(crate) fn after(c: &Compiled, pos: usize) -> usize {
    let mut next = pos + 1;
    let mut block = c.steps[pos].block;
    let mut section = c.steps[pos].section;
    while let Some(id) = block {
        let span = &c.blocks[id];
        let end = match section {
            Section::Body => span.body.end,
            Section::Rescue => span.rescue.end,
            Section::Always => span.always.end,
        };
        if next != end {
            return next;
        }
        // The section ran to its end. `always` follows a body that succeeded - stepping over
        // the rescue, which is for failures - and follows a rescue directly.
        if section != Section::Always && !span.always.is_empty() {
            return span.always.start;
        }
        // This block is finished. Its own last index is where its `always` ends, and the block
        // that holds it may end at exactly that index too, so the walk goes on rather than
        // returning: a block ending where its parent's body ends must still step over the
        // parent's rescue.
        next = span.always.end;
        section = span.section;
        block = span.parent;
    }
    next
}

/// Where a host goes when the task at `pos` has just failed: the `always` section of the
/// innermost block that still has one to run, or `None` when the play is over for this host.
///
/// Measured on ansible-core 2.19.12: a task failing inside a nested block runs the inner
/// `always`, then the outer `always`, then leaves the play - the step after the outer block
/// never runs and the recap counts the two `always` tasks as `ok`. A task failing **inside** an
/// `always` does not finish that section: the rest of it is skipped and the host leaves.
pub(crate) fn after_failure(c: &Compiled, pos: usize) -> Option<usize> {
    let mut block = c.steps[pos].block;
    let mut section = c.steps[pos].section;
    while let Some(id) = block {
        let span = &c.blocks[id];
        if section != Section::Always && !span.always.is_empty() {
            return Some(span.always.start);
        }
        section = span.section;
        block = span.parent;
    }
    None
}

/// The index a host with a failure already pending moves to after finishing `pos`: on through
/// the `always` section it is working its way down, or out to the next one. The steps of an
/// `always` run in order whatever the body did, which is the whole point of the section.
pub(crate) fn after_pending(c: &Compiled, pos: usize) -> Option<usize> {
    let step = &c.steps[pos];
    if let Some(id) = step.block
        && step.section == Section::Always
        && pos + 1 < c.blocks[id].always.end
    {
        return Some(pos + 1);
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
        let mut pos = after_failure(&c, 2).expect("the nested always runs");
        assert_eq!(c.steps[pos].task.name, "nested always");
        pos = after_pending(&c, pos).expect("the outer always runs next");
        assert_eq!(c.steps[pos].task.name, "outer always");
        assert_eq!(after_pending(&c, pos), None, "and then the host is done");
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
        let first = c.blocks[0].always.start;
        assert_eq!(c.steps[first].task.name, "first cleanup");
        assert_eq!(
            after_pending(&c, first).map(|p| c.steps[p].task.name.as_str()),
            Some("second cleanup"),
            "a pending failure still finishes the section"
        );
        assert_eq!(
            after_failure(&c, first),
            None,
            "a failure raised here ends the section instead"
        );
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
