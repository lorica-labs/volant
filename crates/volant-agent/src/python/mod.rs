// SPDX-License-Identifier: GPL-3.0-or-later
//! One warm Python server per interpreter and payload, forking a child per module.
//!
//! The server is where this release's Python support earns its name. Measured on the reference's
//! `ping`: a module started from cold takes 340 ms, forking alone takes it to 266 ms, and a
//! parent that has already imported `module_utils` from the payload takes it to 12.8 ms. The
//! import is paid once per run, by a parent that never runs a module's own code, so nothing one
//! module did reaches the next. What a module imports from the payload and the standard library
//! is imported in the parent too, once its first result is sent, so its later tasks skip that
//! part.
//!
//! The parent and this side speak the agent's own frames - four bytes of big-endian length, then
//! JSON - so there is one framing in the whole agent rather than two.

mod frame;
mod pool;
mod server;

pub use pool::{batch_started, run, warm};
#[cfg(all(test, unix))]
use server::Server;
pub use server::set_perf;

/// Sets this process's soft limit on open files to `wanted`, never past the hard limit, as
/// `set_open_file_limit` in the server does for a Python module. Called between fork and exec,
/// so it does nothing but the two calls, and a refusal leaves the limit as it was.
#[cfg(unix)]
pub(crate) fn set_open_file_limit(wanted: u64) {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes into the struct it is given, which lives for the whole call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    let wanted = (wanted as libc::rlim_t).min(limit.rlim_max);
    if wanted != limit.rlim_cur {
        limit.rlim_cur = wanted;
        // SAFETY: as above; the struct is only read.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    }
}

/// Where a Python task's time went, as the server measured it: the fork, the module's import up
/// to the moment it builds its `AnsibleModule`, and the rest of its run. Each is `None` when the
/// server did not report it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub fork: Option<u64>,
    pub import: Option<u64>,
    pub module: Option<u64>,
}

#[cfg(all(test, unix))]
mod tests;
