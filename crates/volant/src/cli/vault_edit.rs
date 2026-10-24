// SPDX-License-Identifier: GPL-3.0-or-later
//! `vault create`, `edit` and `rekey`: the editor run on a private temporary copy, and the file
//! encrypted again with the secret and label the reference picks.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::vault::{
    Failure, bad_option, encrypt_secret, fail, named, os_error, read_data, realpath, write_data,
};
use crate::vault::{self, Request, Secrets, VaultError};

/// A setting the way ansible-core resolves it: of the environment names the last one set wins,
/// then `[defaults] key`.
pub(super) fn setting(envs: &[&str], key: &str) -> Option<String> {
    envs.iter()
        .rev()
        .find_map(|name| std::env::var(name).ok())
        .or_else(|| crate::config::defaults_value(key))
}

/// The controller's temporary directory: `ANSIBLE_LOCAL_TEMP`, `local_tmp`, else `tmp` under
/// `ANSIBLE_HOME` or `~/.ansible`.
fn local_tmp() -> Result<PathBuf, Failure> {
    let home = || {
        home_dir().ok_or_else(|| {
            fail("Unable to find the home directory for the temporary files: HOME is not set")
        })
    };
    let chosen = setting(&["ANSIBLE_LOCAL_TEMP"], "local_tmp").or_else(|| {
        std::env::var("ANSIBLE_HOME")
            .ok()
            .map(|h| format!("{h}/tmp"))
    });
    Ok(match chosen {
        Some(path) => match path.strip_prefix("~/") {
            Some(rest) => home()?.join(rest),
            None => PathBuf::from(path),
        },
        None => home()?.join(".ansible/tmp"),
    })
}

/// `HOME`, else the home directory of the user's account, as Python's `expanduser` has it.
fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        // SAFETY: `getpwuid` returns null or a record that stays valid until the next call; the
        // directory is copied out at once.
        unsafe {
            let entry = libc::getpwuid(libc::getuid());
            if !entry.is_null() && !(*entry).pw_dir.is_null() {
                let dir = std::ffi::CStr::from_ptr((*entry).pw_dir);
                return Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())));
            }
        }
    }
    None
}

/// Zeros over a file, so the plaintext is not left in the blocks the removal frees.
fn scrub(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path)
        && meta.is_file()
        && let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(path)
    {
        let zeros = vec![0_u8; usize::try_from(meta.len()).unwrap_or(0)];
        let _ = file.write_all(&zeros).and_then(|()| file.sync_all());
    }
}

/// The plaintext copy the editor works on: a 0600 file in a private directory that is gone, its
/// bytes zeroed first, on every way out of the function that made it.
struct Scratch {
    file: tempfile::NamedTempFile,
    _dir: tempfile::TempDir,
}

impl Scratch {
    fn new(suffix: &str, content: &[u8]) -> Result<Self, Failure> {
        let base = local_tmp()?;
        let made = (|| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&base)?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir_all(&base)?;
            let dir = tempfile::Builder::new()
                .prefix("ansible-local-")
                .tempdir_in(&base)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
            }
            let mut file = tempfile::Builder::new()
                .suffix(suffix)
                .tempfile_in(dir.path())?;
            file.write_all(content)?;
            file.flush()?;
            Ok::<_, std::io::Error>(Self { file, _dir: dir })
        })();
        made.map_err(|e| fail(format!("Unable to create the temporary file: {e}")))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        scrub(self.file.path());
    }
}

#[cfg(unix)]
static CAUGHT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn note_signal(signal: libc::c_int) {
    CAUGHT.store(signal, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(unix)]
const WATCHED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// The handlers in place from before the plaintext copy exists until the file is written: Ctrl-C
/// belongs to the editor, SIGTERM and SIGHUP are noted and acted on at the next check, so the
/// copy is removed by the code that made it. The old handlers come back on drop.
struct Signals(#[cfg(unix)] [libc::sighandler_t; 3]);

impl Signals {
    #[cfg(unix)]
    fn install() -> Self {
        CAUGHT.store(0, std::sync::atomic::Ordering::SeqCst);
        let note = note_signal as *const () as libc::sighandler_t;
        let handlers = [libc::SIG_IGN, note, note];
        // SAFETY: plain handler changes; `note_signal` only stores to an atomic.
        let old = std::array::from_fn(|i| unsafe { libc::signal(WATCHED[i], handlers[i]) });
        Self(old)
    }

    #[cfg(not(unix))]
    fn install() -> Self {
        Self()
    }

    /// An error ending the command when SIGTERM or SIGHUP has arrived: 128 plus the signal.
    fn check(&self) -> Result<(), Failure> {
        #[cfg(unix)]
        {
            let signal = CAUGHT.load(std::sync::atomic::Ordering::SeqCst);
            if signal != 0 {
                return Err(Failure {
                    message: format!("Interrupted by signal {signal}"),
                    code: 128 + signal,
                });
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for Signals {
    fn drop(&mut self) {
        for (signal, old) in WATCHED.iter().zip(self.0) {
            // SAFETY: puts back what `install` replaced.
            unsafe { libc::signal(*signal, old) };
        }
    }
}

/// Runs the editor on `path` and waits. A signal noted while it runs stops it. The editor's exit
/// status is not looked at, as in the reference.
fn run_editor(path: &Path, signals: &Signals) -> Result<(), Failure> {
    let editor = setting(&["ANSIBLE_EDITOR", "EDITOR"], "editor").unwrap_or_else(|| "vi".into());
    let mut argv = shlex::split(&editor)
        .ok_or_else(|| fail(format!("No closing quotation in the editor: {editor}")))?;
    argv.push(path.to_string_lossy().into_owned());
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: `signal` is async-signal-safe and the closure touches nothing else.
        unsafe {
            command.pre_exec(|| {
                for signal in WATCHED {
                    libc::signal(signal, libc::SIG_DFL);
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn().map_err(|e| {
        fail(format!(
            "Unable to execute the command \"{}\": {}",
            argv.join(" "),
            os_error(&argv[0], &e)
        ))
    })?;
    loop {
        // The signal first: one sent to the whole process group ends the editor too, and that is
        // a request to stop, not an editor that finished.
        if let Err(stop) = signals.check() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(stop);
        }
        if !matches!(child.try_wait(), Ok(None)) {
            return signals.check();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The mode and owner a file had, put back on its replacement.
fn restore(path: &str, before: &std::fs::Metadata) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(before.mode()));
        if let Ok(c) = std::ffi::CString::new(Path::new(path).as_os_str().as_bytes()) {
            // SAFETY: a NUL-terminated path that lives through the call.
            unsafe { libc::chown(c.as_ptr(), before.uid(), before.gid()) };
        }
    }
    #[cfg(not(unix))]
    let _ = (path, before);
}

/// `VaultEditor._edit_file_helper`: the editor on a copy, and the file replaced when the copy
/// changed. `existing` is `None` for a new file, which is always written.
fn edit_helper(
    path: &str,
    secret: &[u8],
    label: Option<&str>,
    existing: Option<&[u8]>,
) -> Result<(), Failure> {
    let suffix = Path::new(&realpath(path))
        .extension()
        .map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
    // Before the copy exists and until the file is written, so no signal finds plaintext on disk
    // with the default action in place.
    let signals = Signals::install();
    let scratch = Scratch::new(&suffix, existing.unwrap_or_default())?;
    signals.check()?;
    run_editor(scratch.file.path(), &signals)?;
    let edited = read_data(&scratch.file.path().to_string_lossy())?;
    signals.check()?;
    if existing == Some(&edited[..]) {
        return Ok(());
    }
    let text = vault::encrypt(&edited, secret, label).map_err(|e| fail(e.0))?;
    let before = std::fs::metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file);
    signals.check()?;
    write_data(text.as_bytes(), path, false)?;
    if let Some(before) = before {
        restore(path, &before);
    }
    signals.check()
}

/// `create`: one new file, edited, then encrypted.
pub(super) fn create(
    files: &[String],
    label: &str,
    secret: &[u8],
    skip_tty_check: bool,
) -> Result<(), Failure> {
    use std::io::IsTerminal as _;
    let [file] = files else {
        return Err(bad_option(
            "ansible-vault create can take only one filename argument",
        ));
    };
    if !(std::io::stdout().is_terminal() || skip_tty_check) {
        return Err(bad_option("not a tty, editor cannot be opened"));
    }
    if let Some(dir) = Path::new(file)
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        && !dir.exists()
    {
        eprintln!("[WARNING]: {} does not exist, creating...", dir.display());
        std::fs::create_dir_all(dir).map_err(|e| fail(os_error(&dir.to_string_lossy(), &e)))?;
    }
    if Path::new(file).is_file() {
        return Err(fail(format!("{file} exists, please use 'edit' instead")));
    }
    if Path::new(file).is_dir() {
        return Err(fail(format!("{file} is a directory")));
    }
    let path = vault::normalize(Path::new(file))
        .to_string_lossy()
        .into_owned();
    edit_helper(&path, secret, Some(label), None)
}

/// The label of a 1.2 header, which a file keeps when it is edited.
fn header_label(data: &[u8]) -> Option<String> {
    let line = data.split(|b| *b == b'\n').next()?;
    let line = String::from_utf8_lossy(line);
    let fields: Vec<&str> = line.trim().split(';').collect();
    match fields[..] {
        [_, "1.2", _, label, ..] if !label.is_empty() => Some(label.to_string()),
        _ => None,
    }
}

/// The plaintext and the secret that opened it, the secrets named like the file's label first.
fn open(data: &[u8], labelled: &[(String, Vec<u8>)]) -> Result<(Vec<u8>, usize), VaultError> {
    let label = header_label(data);
    let mut order: Vec<usize> = (0..labelled.len()).collect();
    order.sort_by_key(|&i| Some(&labelled[i].0) != label.as_ref());
    let mut last = None;
    for i in order {
        match vault::decrypt(data, &Secrets::new(vec![labelled[i].1.clone()])) {
            Ok(plain) => return Ok((plain, i)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| vault::decrypt(data, &Secrets::default()).unwrap_err()))
}

/// `edit`: each file decrypted, edited and encrypted again with the secret that opened it and
/// the label it had.
pub(super) fn edit(files: &[String], labelled: &[(String, Vec<u8>)]) -> Result<(), Failure> {
    for file in files {
        let path = realpath(file);
        let data = read_data(&path)?;
        let (plain, used) = open(&data, labelled).map_err(|e| fail(named(&e, &path)))?;
        edit_helper(
            &path,
            &labelled[used].1,
            header_label(&data).as_deref(),
            Some(&plain),
        )?;
    }
    Ok(())
}

/// Where the new secret of `rekey` comes from.
pub(super) struct NewSecret {
    pub(super) ask: bool,
    pub(super) file: Option<PathBuf>,
    pub(super) id: Option<String>,
    pub(super) wanted: Option<String>,
}

/// `rekey`: each file decrypted with the old secrets and written again with the new one.
pub(super) fn rekey(
    files: &[String],
    labelled: &[(String, Vec<u8>)],
    new: NewSecret,
    config: &crate::config::Config,
) -> Result<(), Failure> {
    // The configured identities join the new ones only when an encrypt id picks among them.
    let mut ids = if new.wanted.is_some() {
        config.vault_identity_list.clone()
    } else {
        Vec::new()
    };
    ids.extend(new.id);
    let fresh = vault::load_labelled(
        &Request {
            ask: new.ask,
            password_files: new.file.into_iter().collect(),
            ids,
            config_file: config.vault_password_file.clone(),
            config_ids: Vec::new(),
        },
        true,
    )
    .map_err(|err| {
        eprintln!("[WARNING]: {}", err.warning);
        fail(err.message)
    })?;
    // Several new secrets and no encrypt id take the first, unlike `encrypt`.
    let (label, secret) = match (&new.wanted, fresh.first()) {
        (None, Some((label, secret))) => (label.as_str(), secret.as_slice()),
        (wanted, _) => encrypt_secret(&fresh, wanted.as_deref())?,
    };
    for file in files {
        let path = realpath(file);
        // A missing file is an exception in the reference, not one of its messages.
        let before = std::fs::metadata(&path).map_err(|e| Failure {
            message: format!(
                "Unexpected Exception, this is probably a bug: {}",
                os_error(&path, &e)
            ),
            code: 250,
        })?;
        let data = read_data(&path)?;
        let (plain, _) = open(&data, labelled).map_err(|e| fail(named(&e, &path)))?;
        let text = vault::encrypt(&plain, secret, Some(label)).map_err(|e| fail(e.0))?;
        write_data(text.as_bytes(), &path, true)?;
        restore(&path, &before);
    }
    eprintln!("Rekey successful");
    Ok(())
}
