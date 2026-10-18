// SPDX-License-Identifier: GPL-3.0-or-later
//! Moving through a compiled play: where a host goes after a step.

use super::model::{Compiled, Section, StepKind};
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
