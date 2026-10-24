// SPDX-License-Identifier: GPL-3.0-or-later
//! The arguments of the `vault` command.

use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Args, Debug)]
#[command(disable_version_flag = true, subcommand_required = false)]
pub struct VaultArgs {
    /// Print the version and exit.
    #[arg(long, exclusive = true)]
    pub(super) version: bool,
    #[command(subcommand)]
    pub(super) command: Option<VaultCommand>,
}

/// Where the secrets come from, and how much to say.
#[derive(Args, Debug)]
pub struct Common {
    /// The vault identity to use, `label@source` where the source is a file, an executable or
    /// `prompt`. Repeatable.
    #[arg(long = "vault-id", value_name = "VAULT_IDS")]
    pub(super) vault_id: Vec<String>,
    /// Ask for the vault password on the terminal.
    #[arg(
        short = 'J',
        long = "ask-vault-pass",
        visible_alias = "ask-vault-password",
        conflicts_with = "vault_password_file"
    )]
    pub(super) ask_vault_pass: bool,
    /// File, or executable printing the password, for the vault. Repeatable.
    #[arg(
        long = "vault-password-file",
        alias = "vault-pass-file",
        value_name = "VAULT_PASSWORD_FILES"
    )]
    pub(super) vault_password_file: Vec<PathBuf>,
    /// Print more: the configuration file at -v, the version at -vv.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub(super) verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum VaultCommand {
    /// Encrypt files in place, or into the file given to --output.
    Encrypt {
        #[command(flatten)]
        common: Common,
        /// Write the vault text here instead of replacing the file; `-` is standard output.
        #[arg(long, value_name = "OUTPUT_FILE", overrides_with = "output")]
        output: Option<String>,
        /// The vault id to encrypt with, required when more than one is given.
        #[arg(
            long,
            value_name = "ENCRYPT_VAULT_ID",
            overrides_with = "encrypt_vault_id"
        )]
        encrypt_vault_id: Option<String>,
        /// The files; none, or `-`, reads standard input.
        #[arg(value_name = "file_name")]
        files: Vec<String>,
    },
    /// Decrypt files in place, or into the file given to --output.
    Decrypt {
        #[command(flatten)]
        common: Common,
        /// Write the plaintext here instead of replacing the file; `-` is standard output.
        #[arg(long, value_name = "OUTPUT_FILE", overrides_with = "output")]
        output: Option<String>,
        /// The files; none, or `-`, reads standard input.
        #[arg(value_name = "file_name")]
        files: Vec<String>,
    },
    /// Open a new file in the editor and encrypt what is saved.
    Create {
        #[command(flatten)]
        common: Common,
        /// The vault id to encrypt with, required when more than one is given.
        #[arg(
            long,
            value_name = "ENCRYPT_VAULT_ID",
            overrides_with = "encrypt_vault_id"
        )]
        encrypt_vault_id: Option<String>,
        /// Open the editor when standard output is not a terminal.
        #[arg(long)]
        skip_tty_check: bool,
        /// The file to create.
        #[arg(value_name = "file_name")]
        files: Vec<String>,
    },
    /// Open encrypted files in the editor and encrypt what is saved.
    Edit {
        #[command(flatten)]
        common: Common,
        /// Accepted as ansible-vault accepts it: the file keeps the label it has.
        #[arg(
            long,
            value_name = "ENCRYPT_VAULT_ID",
            overrides_with = "encrypt_vault_id"
        )]
        encrypt_vault_id: Option<String>,
        /// The files.
        #[arg(value_name = "file_name")]
        files: Vec<String>,
    },
    /// Encrypt files again with a new password or label.
    Rekey {
        #[command(flatten)]
        common: Common,
        /// The vault id to encrypt with, when more than one new one is given.
        #[arg(
            long,
            value_name = "ENCRYPT_VAULT_ID",
            overrides_with = "encrypt_vault_id"
        )]
        encrypt_vault_id: Option<String>,
        /// The file holding the new password, or an executable that prints it.
        #[arg(
            long,
            value_name = "NEW_VAULT_PASSWORD_FILE",
            conflicts_with = "new_vault_id",
            overrides_with = "new_vault_password_file"
        )]
        new_vault_password_file: Option<PathBuf>,
        /// The new vault identity, `label@source`.
        #[arg(long, value_name = "NEW_VAULT_ID", overrides_with = "new_vault_id")]
        new_vault_id: Option<String>,
        /// The files.
        #[arg(value_name = "file_name")]
        files: Vec<String>,
    },
    /// Print the plaintext of encrypted files, through the pager on a terminal.
    View {
        #[command(flatten)]
        common: Common,
        /// The files.
        #[arg(value_name = "file_name")]
        files: Vec<String>,
    },
}
