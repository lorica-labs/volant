// SPDX-License-Identifier: GPL-3.0-or-later
//! Playbook loading: plays and tasks, with the keywords this release supports.

mod connect;
mod deferred;
mod fields;
mod import;
mod keywords;
mod load;
mod model;
mod origin;
mod play;
mod splat;
mod task;

pub use deferred::Deferred;
pub use fields::Debugger;
pub use keywords::{BECOME_METHODS, Flag, META, is_meta};
pub(crate) use keywords::{
    deprecate_play_hosts, names_no_action, possibly_all_template, print_deprecation,
    print_deprecation_line, python_repr, set_deprecation_warnings, stray_raw_params,
};
pub use load::{Rendering, load, load_rendering, parse};
pub(crate) use load::{TaskFileRefused, base_dir, parse_handlers_file, parse_tasks_file};
pub use model::{Apply, Block, HandlerTask, Play, PlayTask, Playbook, TaskOrBlock, Throttle};
pub(crate) use origin::Located;
pub use origin::Origin;
pub(crate) use play::parse_role_entries;

#[cfg(test)]
mod tests;
