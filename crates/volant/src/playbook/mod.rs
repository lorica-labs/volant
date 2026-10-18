// SPDX-License-Identifier: GPL-3.0-or-later
//! Playbook loading: plays and tasks, with the keywords this release supports.

mod fields;
mod keywords;
mod load;
mod model;
mod origin;
mod play;
mod task;

pub use fields::Debugger;
pub use keywords::{BECOME_METHODS, Flag, META, is_meta};
pub(crate) use keywords::{
    possibly_all_template, print_deprecation, python_repr, stray_raw_params,
};
pub(crate) use load::{TaskFileRefused, base_dir, parse_handlers_file, parse_tasks_file};
pub use load::{load, parse};
pub use model::{Block, HandlerTask, Play, PlayTask, Playbook, TaskOrBlock, Throttle};
pub use origin::Origin;
pub(crate) use play::parse_role_entries;

#[cfg(test)]
mod tests;
