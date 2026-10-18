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

mod builder;
mod compiled;
mod flatten;
mod include;
mod inherit;
mod model;
mod nav;
mod tags;

pub(crate) use builder::DEPTH;
pub(crate) use compiled::{
    META_ACTIONS, batches, beside_or_in_role, handler_steps, meta_action, resolve_notify,
};
pub(crate) use flatten::compile;
pub(crate) use include::{
    Grafted, IncludeRequest, IncludeTarget, expand_include, graft, include_role_entry,
};
pub(crate) use model::{
    Compiled, Handler, IncludeKind, IncludeParams, Origin, Section, Step, StepKind, is_splice_point,
};
pub(crate) use nav::{
    after, after_failure, after_pending, find_task, first, listed, rescue_target,
};
pub(crate) use tags::TagSelection;

#[cfg(test)]
mod tests;
