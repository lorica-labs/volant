// SPDX-License-Identifier: GPL-3.0-or-later
//! Ansible's filters, tests and lookups on top of MiniJinja's Jinja2 builtins.

use std::path::PathBuf;

use minijinja::Environment;

pub fn register(_env: &mut Environment<'static>, _base_dir: PathBuf) {}
