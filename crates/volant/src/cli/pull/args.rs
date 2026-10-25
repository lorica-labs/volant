// SPDX-License-Identifier: GPL-3.0-or-later
//! The arguments of `ansible-pull`.

use std::path::PathBuf;

use clap::Parser;

use crate::cli::args::{ConnectionArgs, HostsArgs, VaultSecretArgs};

/// The arguments of `ansible-pull`.
#[derive(Parser, Debug)]
#[command(disable_version_flag = true)]
pub struct PullArgs {
    /// Playbooks in the checkout to run; by default `<fqdn>.yml`, `<hostname>.yml` or
    /// `local.yml`, the first that exists.
    #[arg(value_name = "playbook.yml")]
    pub playbooks: Vec<String>,
    /// URL of the playbook repository.
    #[arg(short = 'U', long = "url", value_name = "URL")]
    pub url: Option<String>,
    /// Directory to check the repository out to; `~/.ansible/pull/<fqdn>` by default.
    #[arg(short = 'd', long = "directory", value_name = "DEST")]
    pub dest: Option<PathBuf>,
    /// Branch, tag or commit to check out.
    #[arg(short = 'C', long = "checkout", value_name = "CHECKOUT")]
    pub checkout: Option<String>,
    /// Do a full clone, instead of a shallow one.
    #[arg(long = "full")]
    pub full: bool,
    /// Repository module: `git`, run natively, or `subversion`, `hg` and `bzr`, run by the module.
    #[arg(short = 'm', long = "module-name", default_value = "git")]
    pub module_name: String,
    /// Remove the checkout after the playbook run.
    #[arg(long = "purge")]
    pub purge: bool,
    /// Only run the playbook if the repository has been updated.
    #[arg(short = 'o', long = "only-if-changed")]
    pub only_if_changed: bool,
    /// Sleep for a random number of seconds below this one before starting.
    #[arg(
        short = 's',
        long = "sleep",
        value_name = "SLEEP",
        allow_hyphen_values = true
    )]
    pub sleep: Option<String>,
    /// Run the playbook even if the repository could not be updated.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    /// Add the host key of the repository's host if it is not known yet.
    #[arg(long = "accept-host-key")]
    pub accept_host_key: bool,
    /// Verify the GPG signature of the checked-out commit; stop if it does not verify.
    #[arg(long = "verify-commit")]
    pub verify_commit: bool,
    /// Discard the modified files of the checkout.
    #[arg(long = "clean")]
    pub clean: bool,
    /// Submodules follow the latest changes of their branch.
    #[arg(long = "track-subs")]
    pub track_subs: bool,
    /// Run the playbook in check mode.
    #[arg(long = "check")]
    pub check: bool,
    /// Show the differences the playbook makes to files.
    #[arg(long = "diff")]
    pub diff: bool,
    #[command(flatten)]
    pub hosts: HostsArgs,
    /// Extra variables, for both steps. Repeatable.
    #[arg(short = 'e', long = "extra-vars", value_name = "EXTRA_VARS")]
    pub extra_vars: Vec<String>,
    /// Only run the tasks carrying these tags.
    #[arg(short = 't', long = "tags", value_name = "TAGS")]
    pub tags: Vec<String>,
    /// Skip the tasks carrying these tags.
    #[arg(long = "skip-tags", value_name = "SKIP_TAGS")]
    pub skip_tags: Vec<String>,
    /// Accepted; the reference does not hand it on.
    #[arg(long = "list-hosts")]
    pub list_hosts: bool,
    /// Clear the fact cache before the playbook run.
    #[arg(long = "flush-cache")]
    pub flush_cache: bool,
    /// Accepted; the reference does not hand it on.
    #[arg(short = 'M', long = "module-path", value_name = "MODULE_PATH")]
    pub module_path: Vec<String>,
    /// Ask for the escalation password.
    #[arg(short = 'K', long = "ask-become-pass")]
    pub ask_become_pass: bool,
    /// Read the escalation password from this file.
    #[arg(
        long = "become-password-file",
        visible_alias = "become-pass-file",
        value_name = "BECOME_PASSWORD_FILE",
        conflicts_with = "ask_become_pass"
    )]
    pub become_password_file: Option<PathBuf>,
    #[command(flatten)]
    pub connect: ConnectionArgs,
    #[command(flatten)]
    pub vault: VaultSecretArgs,
    /// More output, for both steps. Repeatable.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Print the version and the paths the program works with, and stop.
    #[arg(long = "version")]
    pub version: bool,
}
