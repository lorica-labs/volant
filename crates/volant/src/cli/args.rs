// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

use clap::Parser;

use crate::python::Facts;

#[derive(Parser, Debug)]
#[command(disable_version_flag = true)]
pub struct PlaybookArgs {
    /// Playbook files, run in order.
    #[arg(required_unless_present = "version", value_name = "PLAYBOOK")]
    pub playbooks: Vec<PathBuf>,
    /// Inventory source: a file, a directory, or a comma-separated host list. Repeatable; the
    /// sources are merged in order.
    #[arg(
        short = 'i',
        long = "inventory",
        alias = "inventory-file",
        value_name = "SOURCE"
    )]
    pub inventory: Vec<String>,
    /// Extra variables: `key=value` pairs, inline JSON or YAML, or `@file`. Repeatable.
    #[arg(short = 'e', long = "extra-vars", value_name = "VARS")]
    pub extra_vars: Vec<String>,
    /// Limit the play to this host pattern.
    #[arg(short = 'l', long = "limit", value_name = "SUBSET")]
    pub limit: Option<String>,
    /// Only run tasks carrying these tags. Comma-separated, repeatable.
    #[arg(short = 't', long = "tags", value_name = "TAGS")]
    pub tags: Vec<String>,
    /// Only run tasks carrying none of these tags. Comma-separated, repeatable.
    #[arg(long = "skip-tags", value_name = "SKIP_TAGS")]
    pub skip_tags: Vec<String>,
    /// List the tasks the run would execute, and stop.
    #[arg(long = "list-tasks")]
    pub list_tasks: bool,
    /// List the tags the run would select, and stop.
    #[arg(long = "list-tags")]
    pub list_tags: bool,
    /// List the hosts each play would run on, and stop.
    #[arg(long = "list-hosts")]
    pub list_hosts: bool,
    /// Load and compile the playbooks without running anything.
    #[arg(long = "syntax-check")]
    pub syntax_check: bool,
    /// Disable coloured output.
    #[arg(long)]
    pub no_color: bool,
    /// Show module results for successful tasks too. Repeatable.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Directories searched for modules before the configured ones. Colon-separated, repeatable.
    #[arg(short = 'M', long = "module-path", value_name = "MODULE_PATH")]
    pub module_path: Vec<String>,
    /// Log in to remote hosts as this user.
    #[arg(short = 'u', long = "user", value_name = "REMOTE_USER")]
    pub user: Option<String>,
    /// Connection plugin to use.
    #[arg(short = 'c', long = "connection", value_name = "CONNECTION")]
    pub connection: Option<String>,
    /// Private key file for ssh authentication.
    #[arg(
        long = "private-key",
        alias = "key-file",
        value_name = "PRIVATE_KEY_FILE"
    )]
    pub private_key: Option<PathBuf>,
    /// Seconds to wait for a connection.
    #[arg(short = 'T', long = "timeout", value_name = "TIMEOUT")]
    pub timeout: Option<u64>,
    /// Number of hosts to run at once.
    #[arg(short = 'f', long = "forks", value_name = "FORKS")]
    pub forks: Option<usize>,
    /// Run tasks with privilege escalation.
    #[arg(short = 'b', long = "become")]
    pub r#become: bool,
    /// Escalate to this user instead of root.
    #[arg(long = "become-user", value_name = "USER")]
    pub become_user: Option<String>,
    /// Escalation method: `sudo` or `su`.
    #[arg(long = "become-method", value_name = "METHOD")]
    pub become_method: Option<String>,
    /// Ask for the escalation password on the terminal.
    #[arg(short = 'K', long = "ask-become-pass")]
    pub ask_become_pass: bool,
    /// Read the escalation password from the first line of this file.
    #[arg(
        long = "become-password-file",
        visible_alias = "become-pass-file",
        value_name = "BECOME_PASSWORD_FILE",
        conflicts_with = "ask_become_pass"
    )]
    pub become_password_file: Option<PathBuf>,
    /// Ask for the connection password on the terminal.
    #[arg(
        short = 'k',
        long = "ask-pass",
        conflicts_with = "connection_password_file"
    )]
    pub ask_pass: bool,
    /// Read the connection password from the first line of this file.
    #[arg(
        long = "connection-password-file",
        visible_alias = "conn-pass-file",
        value_name = "CONNECTION_PASSWORD_FILE"
    )]
    pub connection_password_file: Option<PathBuf>,
    /// Arguments added to every `ssh` command line.
    #[arg(long = "ssh-common-args", value_name = "SSH_COMMON_ARGS")]
    pub ssh_common_args: Option<String>,
    /// Arguments added to the `ssh` command lines only.
    #[arg(long = "ssh-extra-args", value_name = "SSH_EXTRA_ARGS")]
    pub ssh_extra_args: Option<String>,
    /// Accepted for `scp`, which Volant does not run: files move over the agent link.
    #[arg(long = "scp-extra-args", value_name = "SCP_EXTRA_ARGS")]
    pub scp_extra_args: Option<String>,
    /// Accepted for `sftp`, which Volant does not run: files move over the agent link.
    #[arg(long = "sftp-extra-args", value_name = "SFTP_EXTRA_ARGS")]
    pub sftp_extra_args: Option<String>,

    /// Ask for the vault password on the terminal.
    #[arg(
        short = 'J',
        long = "ask-vault-pass",
        visible_alias = "ask-vault-password"
    )]
    pub ask_vault_pass: bool,
    /// File, or executable printing the password, for the vault. Repeatable.
    #[arg(
        long = "vault-password-file",
        alias = "vault-pass-file",
        value_name = "VAULT_PASSWORD_FILE"
    )]
    pub vault_password_file: Vec<PathBuf>,
    /// Vault identity, `label@source` where the source is a file, an executable or `prompt`.
    /// Repeatable.
    #[arg(long = "vault-id", value_name = "VAULT_IDS")]
    pub vault_id: Vec<String>,

    /// Run handlers even if a task fails.
    #[arg(long = "force-handlers")]
    pub force_handlers: bool,

    /// Don't make any changes; instead, try to predict some of the changes that may occur.
    #[arg(short = 'C', long = "check")]
    pub check: bool,
    /// When changing (small) files and templates, show the differences in those files; works
    /// great with --check.
    #[arg(short = 'D', long = "diff")]
    pub diff: bool,

    /// Print where the run's time went on stderr, after the recap.
    #[arg(long = "profile")]
    pub profile: bool,
    /// Where `setup` gets its facts: `python` forces the Python module, `native` the agent's own
    /// collector, `auto` picks the native one only when the plays provably read nothing else.
    #[arg(long = "facts", value_enum, default_value_t = Facts::Auto)]
    pub facts: Facts,

    /// Start the run at the first task with this name; `*` and `?` match any text.
    #[arg(long = "start-at-task", value_name = "START_AT_TASK")]
    pub start_at_task: Option<String>,
    /// Ask before each task whether to run it. Needs a terminal.
    #[arg(long = "step")]
    pub step: bool,
    /// Remove the fact cache entries of the inventory hosts before the run.
    #[arg(long = "flush-cache")]
    pub flush_cache: bool,
    /// Print the version and the ansible-core release it matches, and stop.
    #[arg(long = "version")]
    pub version: bool,
}
