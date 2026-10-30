// SPDX-License-Identifier: GPL-3.0-or-later
//! The colour and deprecation settings, read from the environment and turned into a choice.

use super::{Config, settings::boolean};

impl Config {
    /// The colour choice of the run. `--no-color` beats `force_color`, which beats `nocolor`
    /// and `NO_COLOR`; otherwise the terminal decides. Measured on ansible-core 2.19.12.
    #[must_use]
    pub fn color_choice(&self, no_color_flag: bool) -> anstream::ColorChoice {
        if no_color_flag {
            anstream::ColorChoice::Never
        } else if self.force_color {
            anstream::ColorChoice::Always
        } else if self.nocolor {
            anstream::ColorChoice::Never
        } else {
            anstream::ColorChoice::Auto
        }
    }
}

/// `ANSIBLE_FORCE_COLOR`, `ANSIBLE_NOCOLOR` and `ANSIBLE_DEPRECATION_WARNINGS` over the file.
pub(super) fn read_env(config: &mut Config) -> anyhow::Result<()> {
    if let Ok(flag) = std::env::var("ANSIBLE_FORCE_COLOR") {
        config.force_color = boolean("COLOR_FORCE", "env: ANSIBLE_FORCE_COLOR", &flag)?;
    }
    if let Ok(flag) = std::env::var("ANSIBLE_NOCOLOR") {
        config.nocolor = boolean("NOCOLOR", "env: ANSIBLE_NOCOLOR", &flag)?;
    }
    if let Ok(path) = std::env::var("ANSIBLE_LOG_PATH") {
        let cwd = std::env::current_dir().unwrap_or_default();
        config.log_path = Some(super::expand_home(path.trim(), &cwd));
    }
    if let Ok(level) = std::env::var("ANSIBLE_LOG_VERBOSITY") {
        let level = super::integer(
            "DEFAULT_LOG_VERBOSITY",
            "env: ANSIBLE_LOG_VERBOSITY",
            level.trim(),
        )?;
        config.log_verbosity = u8::try_from(level.max(0)).unwrap_or(u8::MAX);
    }
    if let Ok(flag) = std::env::var("ANSIBLE_DEPRECATION_WARNINGS") {
        config.deprecation_warnings = boolean(
            "DEPRECATION_WARNINGS",
            "env: ANSIBLE_DEPRECATION_WARNINGS",
            &flag,
        )?;
    }
    Ok(())
}
