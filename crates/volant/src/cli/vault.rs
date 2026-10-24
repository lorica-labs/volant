// SPDX-License-Identifier: GPL-3.0-or-later
//! The `vault` command: `encrypt`, `decrypt` and `view` of `ansible-vault`, with its messages
//! and exit codes (1 for an error of the data, 5 for a bad option).

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use super::announce::{config_line, version_line};
use crate::vault::{self, Request, Secrets};

#[derive(Args, Debug)]
#[command(disable_version_flag = true, subcommand_required = false)]
pub struct VaultArgs {
    /// Print the version and exit.
    #[arg(long, exclusive = true)]
    version: bool,
    #[command(subcommand)]
    command: Option<VaultCommand>,
}

/// Where the secrets come from, and how much to say.
#[derive(Args, Debug)]
pub struct Common {
    /// The vault identity to use, `label@source` where the source is a file, an executable or
    /// `prompt`. Repeatable.
    #[arg(long = "vault-id", value_name = "VAULT_IDS")]
    vault_id: Vec<String>,
    /// Ask for the vault password on the terminal.
    #[arg(
        short = 'J',
        long = "ask-vault-pass",
        visible_alias = "ask-vault-password",
        conflicts_with = "vault_password_file"
    )]
    ask_vault_pass: bool,
    /// File, or executable printing the password, for the vault. Repeatable.
    #[arg(
        long = "vault-password-file",
        alias = "vault-pass-file",
        value_name = "VAULT_PASSWORD_FILES"
    )]
    vault_password_file: Vec<PathBuf>,
    /// Print more: the configuration file at -v, the version at -vv.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum VaultCommand {
    /// Encrypt files in place, or into the file given to --output.
    Encrypt {
        #[command(flatten)]
        common: Common,
        /// Write the vault text here instead of replacing the file; `-` is standard output.
        #[arg(long, value_name = "OUTPUT_FILE")]
        output: Option<String>,
        /// The vault id to encrypt with, required when more than one is given.
        #[arg(long, value_name = "ENCRYPT_VAULT_ID")]
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
        #[arg(long, value_name = "OUTPUT_FILE")]
        output: Option<String>,
        /// The files; none, or `-`, reads standard input.
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

/// A message for `[ERROR]:` and the exit code it ends the command with.
struct Failure {
    message: String,
    code: i32,
}

fn fail(message: impl Into<String>) -> Failure {
    Failure {
        message: message.into(),
        code: 1,
    }
}

/// `AnsibleOptionsError`, which exits 5.
fn bad_option(message: impl Into<String>) -> Failure {
    Failure {
        message: message.into(),
        code: 5,
    }
}

/// Runs a `vault` subcommand and returns the exit code.
pub fn vault(args: VaultArgs) -> i32 {
    if args.version {
        println!(
            "ansible-vault [core {}]",
            crate::vars::ansible_version()["full"]
                .as_str()
                .unwrap_or_default()
        );
        println!("{}", version_line());
        return 0;
    }
    let Some(command) = args.command else {
        // `--version` is the one argument that stands without a subcommand, which clap's own
        // `subcommand_required` cannot express, so the usage error is worded here, as argparse
        // words it, with the program as started.
        let started = std::env::args_os()
            .next()
            .and_then(|p| {
                Path::new(&p)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .filter(|name| name.starts_with("ansible-"))
            .unwrap_or_else(|| "volant vault".to_string());
        eprintln!(
            "usage: {started} [-h] [--version] [-v] {{encrypt,decrypt,view}} ...
{started}: error: the following arguments are required: action"
        );
        return 2;
    };
    match execute(command) {
        Ok(()) => 0,
        Err(failure) => {
            eprintln!("[ERROR]: {}", failure.message);
            failure.code
        }
    }
}

fn execute(command: VaultCommand) -> Result<(), Failure> {
    let (common, output, files) = match &command {
        VaultCommand::Encrypt {
            common,
            output,
            files,
            ..
        }
        | VaultCommand::Decrypt {
            common,
            output,
            files,
        } => (common, output.as_deref(), files),
        VaultCommand::View { common, files } => (common, None, files),
    };
    if let Some(id) = common.vault_id.iter().find(|id| id.contains(';')) {
        return Err(bad_option(format!(
            "'{id}' is not a valid vault id. The character ';' is not allowed in vault ids"
        )));
    }
    if output.is_some() && files.len() > 1 {
        return Err(bad_option(
            "At most one input file may be used with the --output option",
        ));
    }
    if common.verbose >= 2 {
        println!("{}", version_line());
    }
    if common.verbose >= 1 {
        println!("{}", config_line());
    }
    let config = crate::config::Config::load().map_err(|e| fail(format!("{e:#}")))?;
    // The reference makes the output path absolute when it reads the command line.
    let output = output.map(|o| {
        if o == "-" {
            o.to_string()
        } else {
            vault::absolute(Path::new(o)).to_string_lossy().into_owned()
        }
    });
    let encrypting = matches!(command, VaultCommand::Encrypt { .. });
    let labelled = vault::load_labelled(
        &Request {
            ask: common.ask_vault_pass,
            password_files: common.vault_password_file.clone(),
            ids: common.vault_id.clone(),
            config_file: config.vault_password_file.clone(),
            config_ids: config.vault_identity_list.clone(),
        },
        encrypting,
    )
    .map_err(|err| {
        eprintln!("[WARNING]: {}", err.warning);
        fail(err.message)
    })?;
    match command {
        VaultCommand::Encrypt {
            encrypt_vault_id,
            files,
            ..
        } => {
            let wanted = encrypt_vault_id.or(config.vault_encrypt_identity);
            let (label, secret) = encrypt_secret(&labelled, wanted.as_deref())?;
            if files.is_empty() && std::io::stdin().is_terminal() {
                eprintln!("Reading plaintext input from stdin");
            }
            for file in inputs(files) {
                encrypt_file(&file, label, secret, output.as_deref())?;
            }
            if std::io::stdout().is_terminal() {
                eprintln!("Encryption successful");
            }
        }
        VaultCommand::Decrypt { files, .. } => {
            let secrets = Secrets::new(labelled.into_iter().map(|(_, secret)| secret).collect());
            if files.is_empty() && std::io::stdin().is_terminal() {
                eprintln!("Reading ciphertext input from stdin");
            }
            for file in inputs(files) {
                let path = realpath(&file);
                let data = read_data(&path)?;
                let plain = vault::decrypt(&data, &secrets).map_err(|e| fail(named(&e, &path)))?;
                write_data(&plain, output.as_deref().unwrap_or(&path), false)?;
            }
            if std::io::stdout().is_terminal() {
                eprintln!("Decryption successful");
            }
        }
        VaultCommand::View { files, .. } => {
            let secrets = Secrets::new(labelled.into_iter().map(|(_, secret)| secret).collect());
            for file in files {
                let data = read_data(&file)?;
                let plain = vault::decrypt(&data, &secrets).map_err(|e| fail(named(&e, &file)))?;
                show(&plain);
            }
        }
    }
    Ok(())
}

/// The files to work on: standard input when none is named.
fn inputs(files: Vec<String>) -> Vec<String> {
    if files.is_empty() {
        vec!["-".to_string()]
    } else {
        files
    }
}

/// The secret that encrypts: the one `--encrypt-vault-id` names, else the only one.
fn encrypt_secret<'a>(
    labelled: &'a [(String, Vec<u8>)],
    wanted: Option<&str>,
) -> Result<(&'a str, &'a [u8]), Failure> {
    if labelled.len() > 1 && wanted.is_none() {
        let labels: Vec<&str> = labelled.iter().map(|(label, _)| label.as_str()).collect();
        return Err(bad_option(format!(
            "The vault-ids {} are available to encrypt. Specify the vault-id to encrypt with --encrypt-vault-id",
            labels.join(",")
        )));
    }
    let found = match wanted {
        Some(id) => labelled.iter().find(|(label, _)| label == id),
        None => labelled.first(),
    };
    match (found, wanted) {
        (Some((label, secret)), _) => Ok((label, secret)),
        (None, Some(id)) => {
            let known: Vec<String> = labelled
                .iter()
                .map(|(label, _)| format!("'{label}'"))
                .collect();
            Err(fail(format!(
                "Did not find a match for --encrypt-vault-id={id} in the known vault-ids [{}]",
                known.join(", ")
            )))
        }
        (None, None) => Err(fail("A vault password is required to use Ansible's Vault")),
    }
}

fn encrypt_file(
    file: &str,
    label: &str,
    secret: &[u8],
    output: Option<&str>,
) -> Result<(), Failure> {
    let path = realpath(file);
    let plain = read_data(&path)?;
    if vault::is_encrypted(&plain) {
        return Err(fail("input is already encrypted"));
    }
    let text = vault::encrypt(&plain, secret, Some(label)).map_err(|e| fail(e.0))?;
    write_data(text.as_bytes(), output.unwrap_or(&path), true)
}

/// A vault error followed by the file it was about, the way the reference words it: the error
/// ends in a full stop.
fn named(error: &vault::VaultError, file: &str) -> String {
    let message = &error.0;
    let stop = if message.ends_with('.') { "" } else { "." };
    format!("{message}{stop} for {file}")
}

/// `os.path.realpath`: links followed as far as the path exists, `-` left alone.
fn realpath(name: &str) -> String {
    if name == "-" {
        return name.to_string();
    }
    let path = vault::normalize(Path::new(name));
    let resolved =
        std::fs::canonicalize(&path).or_else(|_| match (path.parent(), path.file_name()) {
            (Some(parent), Some(leaf)) => std::fs::canonicalize(parent).map(|p| p.join(leaf)),
            _ => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        });
    resolved.unwrap_or(path).to_string_lossy().into_owned()
}

/// An OS error the way Python words it: `[Errno 2] No such file or directory: 'path'`.
fn os_error(path: &str, error: &std::io::Error) -> String {
    let text = error.to_string();
    match error.raw_os_error() {
        Some(errno) => {
            let reason = text
                .rsplit_once(" (os error")
                .map_or(text.as_str(), |(r, _)| r);
            format!("[Errno {errno}] {reason}: '{path}'")
        }
        None => text,
    }
}

fn read_data(name: &str) -> Result<Vec<u8>, Failure> {
    let (shown, read) = if name == "-" {
        let mut bytes = Vec::new();
        (
            name.to_string(),
            std::io::stdin()
                .lock()
                .read_to_end(&mut bytes)
                .map(|_| bytes),
        )
    } else {
        let path = vault::normalize(Path::new(name))
            .to_string_lossy()
            .into_owned();
        let read = std::fs::read(&path);
        (path, read)
    };
    read.map_err(|e| {
        fail(format!(
            "Unable to read source file ({shown}): {}",
            os_error(&shown, &e)
        ))
    })
}

#[cfg(unix)]
fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::CString::new(dir.as_os_str().as_bytes())
        // SAFETY: the path is a NUL-terminated string that lives through the call.
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
}

#[cfg(not(unix))]
fn writable(_dir: &Path) -> bool {
    true
}

/// `VaultEditor.write_data`: standard output for `-`, else a new file of mode 0600 made after
/// the old one is gone, its bytes overwritten first when `shred` is set.
fn write_data(data: &[u8], target: &str, shred: bool) -> Result<(), Failure> {
    if target == "-" {
        let mut out = std::io::stdout().lock();
        return out
            .write_all(data)
            .and_then(|()| out.flush())
            .map_err(|e| fail(format!("Unable to write to standard output: {e}")));
    }
    let path = Path::new(target);
    let dir = path.parent().unwrap_or(Path::new(""));
    if !writable(dir) {
        return Err(fail(format!(
            "Destination '{}' not writable",
            dir.display()
        )));
    }
    // The new bytes go to a file beside the target and replace it by a rename, so a failed
    // create or write leaves the original as it was.
    let leaf = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let temp = dir.join(format!(".{leaf}.volant-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            fail("Vault file got recreated while we were operating on it.")
        } else {
            fail("Problem creating temporary vault file.")
        }
    })?;
    let written = (|| {
        #[cfg(unix)]
        {
            // The reference runs under a umask of 077; the mode is the same whatever ours is.
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(data)?;
        file.sync_all()
    })();
    // The old bytes, held open so they can be zeroed once the new file is in place.
    let old = (shred && path.is_file() && !path.is_symlink())
        .then(|| std::fs::OpenOptions::new().write(true).open(path).ok())
        .flatten();
    let old_len = old
        .as_ref()
        .and_then(|f| f.metadata().ok())
        .map_or(0, |m| m.len());
    let done = written
        .map_err(|e| fail(format!("Unable to write to temporary vault file: {e}")))
        .and_then(|()| {
            std::fs::rename(&temp, path)
                .map_err(|e| fail(format!("Unable to replace {target}: {e}")))
        });
    if let Err(failure) = done {
        let _ = std::fs::remove_file(&temp);
        return Err(failure);
    }
    if let Some(mut old) = old {
        // One pass of zeros over the replaced file: the reference's `shred` fallback writes
        // random passes, and neither survives a copy-on-write filesystem.
        let zeros = vec![0_u8; usize::try_from(old_len).unwrap_or(0)];
        let _ = old.write_all(&zeros).and_then(|()| old.sync_all());
    }
    Ok(())
}

/// The plaintext on the screen: on a terminal through the pager, which defaults to `less`;
/// otherwise as is, with the line break `display` adds.
fn show(plain: &[u8]) {
    let mut out = std::io::stdout().lock();
    if !out.is_terminal() {
        let _ = out.write_all(plain).and_then(|()| out.write_all(b"\n"));
        let _ = out.flush();
        return;
    }
    // `ANSIBLE_PAGER` before `PAGER`; the `pager` setting of `ansible.cfg` is not read.
    let pager = ["ANSIBLE_PAGER", "PAGER"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|p| !p.is_empty()))
        .unwrap_or_else(|| "less".to_string());
    let pager = pager.as_str();
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg(pager)
        .stdin(std::process::Stdio::piped());
    if pager.contains("less") {
        command.env("LESS", "FRSX");
    }
    // A pager that cannot start, or that closes early, ends the listing without an error, as
    // in the reference.
    if let Ok(mut child) = command.spawn() {
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(plain);
        }
        let _ = child.wait();
    }
}
