// SPDX-License-Identifier: GPL-3.0-or-later
//! Ansible Vault: the 1.1 and 1.2 formats of ansible-core 2.19.12, read and written, and the
//! secrets a run is given for them. A secret lives in memory only: it is never logged, never
//! passed to a child process and never sent to an agent.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use aes::Aes256;
use ctr::Ctr128BE;
use ctr::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use sha2::Sha256;

const HEADER: &[u8] = b"$ANSIBLE_VAULT";
const CIPHER: &str = "AES256";
const ITERATIONS: u32 = 10_000;
const WIDTH: usize = 80;

type Aes256Ctr = Ctr128BE<Aes256>;

/// What went wrong with vault data, in the reference's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultError(pub String);

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VaultError {}

/// The passwords of a run. Every one is tried on every piece of data, whatever its label: the
/// reference does not match labels strictly (measured).
#[derive(Default, Clone)]
pub struct Secrets(Vec<Vec<u8>>);

impl Secrets {
    pub fn new(passwords: Vec<Vec<u8>>) -> Self {
        Self(passwords)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The first secret, the one the `vault` filter and `encrypt` use when none is named.
    pub fn first(&self) -> Option<&[u8]> {
        self.0.first().map(Vec::as_slice)
    }
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secrets({} redacted)", self.0.len())
    }
}

static RUN_SECRETS: RwLock<Option<Arc<Secrets>>> = RwLock::new(None);

/// Makes the secrets of this run readable by the templating and file loading. A later call
/// replaces the earlier set.
pub fn install(secrets: Secrets) {
    *RUN_SECRETS.write().expect("secrets lock") = Some(Arc::new(secrets));
}

/// The secrets of this run; empty when none were given.
pub fn run_secrets() -> Arc<Secrets> {
    RUN_SECRETS
        .read()
        .expect("secrets lock")
        .clone()
        .unwrap_or_default()
}

/// Whether `data` starts like vault data.
pub fn is_encrypted(data: &[u8]) -> bool {
    data.starts_with(HEADER)
}

/// Python's `binascii.unhexlify`, with its messages.
fn unhex(text: &[u8]) -> Result<Vec<u8>, VaultError> {
    let fail = |why: &str| VaultError(format!("Vault format unhexlify error: {why}"));
    if text.len() % 2 == 1 {
        return Err(fail("Odd-length string"));
    }
    text.chunks(2)
        .map(|pair| {
            let digit = |b: u8| char::from(b).to_digit(16);
            match (digit(pair[0]), digit(pair[1])) {
                (Some(h), Some(l)) => Ok(u8::try_from(h * 16 + l).expect("two hex digits")),
                _ => Err(fail("Non-hexadecimal digit found")),
            }
        })
        .collect()
}

/// CR and LF removed at both ends, as the reference strips a script's output.
fn strip_crlf(bytes: &[u8]) -> &[u8] {
    let is_eol = |b: &u8| *b == b'\r' || *b == b'\n';
    let start = bytes.iter().position(|b| !is_eol(b)).unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !is_eol(b))
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The pieces of vault data that need no secret to read.
struct Envelope {
    cipher: String,
    /// The outer hex decoded: `hex(salt) "\n" hex(hmac) "\n" hex(ciphertext)`.
    body: Vec<u8>,
}

fn envelope(data: &[u8]) -> Result<Envelope, VaultError> {
    if !is_encrypted(data) {
        return Err(VaultError("Input is not vault encrypted data.".into()));
    }
    let mut lines = data.split(|b| *b == b'\n');
    let header = lines.next().unwrap_or_default();
    let header = String::from_utf8_lossy(header);
    let fields: Vec<&str> = header.trim().split(';').collect();
    let Some(cipher) = fields.get(2) else {
        return Err(VaultError(
            "Vault vaulttext format error: the header has no cipher".into(),
        ));
    };
    let hexed: Vec<u8> = lines
        .flat_map(|l| l.iter().copied().filter(|b| !b.is_ascii_whitespace()))
        .collect();
    Ok(Envelope {
        cipher: (*cipher).to_string(),
        body: unhex(&hexed)?,
    })
}

struct Parts {
    salt: Vec<u8>,
    mac: Vec<u8>,
    ciphertext: Vec<u8>,
}

/// Salt, HMAC and ciphertext of an envelope's body.
fn parts(body: &[u8]) -> Result<Parts, VaultError> {
    let pieces: Vec<&[u8]> = body.splitn(3, |b| *b == b'\n').collect();
    let [salt, mac, ciphertext] = pieces[..] else {
        return Err(VaultError(format!(
            "Vault vaulttext format error: not enough values to unpack (expected 3, got {})",
            pieces.len()
        )));
    };
    Ok(Parts {
        salt: unhex(salt)?,
        mac: unhex(mac)?,
        ciphertext: unhex(ciphertext)?,
    })
}

/// PBKDF2-HMAC-SHA256, 10000 rounds, 80 bytes: AES key, HMAC key, counter start.
fn keys(secret: &[u8], salt: &[u8]) -> ([u8; 32], [u8; 32], [u8; 16]) {
    let mut derived = [0u8; 80];
    pbkdf2::pbkdf2_hmac::<Sha256>(secret, salt, ITERATIONS, &mut derived);
    let key: [u8; 32] = derived[..32].try_into().expect("80 bytes");
    let mac: [u8; 32] = derived[32..64].try_into().expect("80 bytes");
    let iv: [u8; 16] = derived[64..].try_into().expect("80 bytes");
    (key, mac, iv)
}

fn hmac_of(key: &[u8; 32], ciphertext: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(ciphertext);
    mac
}

/// The plaintext when `secret` is the right one, `None` when the HMAC refuses it.
fn open(secret: &[u8], salt: &[u8], mac: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
    let (key, mac_key, iv) = keys(secret, salt);
    hmac_of(&mac_key, ciphertext).verify_slice(mac).ok()?;
    let mut plain = ciphertext.to_vec();
    Aes256Ctr::new(&key.into(), &iv.into()).apply_keystream(&mut plain);
    let pad = usize::from(*plain.last()?);
    if pad == 0 || pad > 16 || pad > plain.len() {
        return None;
    }
    plain.truncate(plain.len() - pad);
    Some(plain)
}

/// Decrypts vault data with the first of `secrets` that fits.
pub fn decrypt(data: &[u8], secrets: &Secrets) -> Result<Vec<u8>, VaultError> {
    let envelope = envelope(data)?;
    if envelope.cipher != CIPHER {
        return Err(VaultError(format!(
            "{} cipher could not be found",
            envelope.cipher
        )));
    }
    if secrets.is_empty() {
        return Err(VaultError(
            "Attempting to decrypt but no vault secrets found".into(),
        ));
    }
    let Parts {
        salt,
        mac,
        ciphertext,
    } = parts(&envelope.body)?;
    secrets
        .0
        .iter()
        .find_map(|secret| open(secret, &salt, &mac, &ciphertext))
        .ok_or_else(|| {
            VaultError("Decryption failed (no vault secrets were found that could decrypt)".into())
        })
}

/// Encrypts `plain` with a fresh random salt. `id` other than `default` gives a 1.2 header.
pub fn encrypt(plain: &[u8], secret: &[u8], id: Option<&str>) -> Result<String, VaultError> {
    use std::io::Read as _;
    let mut random = Vec::with_capacity(32);
    std::fs::File::open("/dev/urandom")
        .and_then(|f| f.take(32).read_to_end(&mut random))
        .map_err(|e| VaultError(format!("could not get a random salt: {e}")))?;
    let salt: [u8; 32] = random
        .try_into()
        .map_err(|_| VaultError("could not get a random salt: short read".into()))?;
    Ok(encrypt_with_salt(plain, secret, id, &salt))
}

pub fn encrypt_with_salt(plain: &[u8], secret: &[u8], id: Option<&str>, salt: &[u8; 32]) -> String {
    let (key, mac_key, iv) = keys(secret, salt);
    let pad = 16 - plain.len() % 16;
    let mut buf = plain.to_vec();
    buf.resize(
        plain.len() + pad,
        u8::try_from(pad).expect("pad is at most 16"),
    );
    Aes256Ctr::new(&key.into(), &iv.into()).apply_keystream(&mut buf);
    let mac = hmac_of(&mac_key, &buf).finalize().into_bytes();
    let body = format!("{}\n{}\n{}", hex(salt), hex(&mac), hex(&buf));
    let hexed = hex(body.as_bytes());
    let header = match id {
        Some(id) if id != "default" => format!("$ANSIBLE_VAULT;1.2;{CIPHER};{id}"),
        _ => format!("$ANSIBLE_VAULT;1.1;{CIPHER}"),
    };
    let mut out = header;
    out.push('\n');
    for line in hexed.as_bytes().chunks(WIDTH) {
        out.push_str(std::str::from_utf8(line).expect("hex is ASCII"));
        out.push('\n');
    }
    out
}

/// Why a secret could not be loaded: the warning line the reference prints first, and the error.
#[derive(Debug, PartialEq, Eq)]
pub struct LoadError {
    pub warning: String,
    pub message: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Source {
    Prompt,
    File(PathBuf),
}

/// `label@source`; a bare `source` has the label `default`. Only `prompt` is not a file.
fn spec(text: &str) -> (String, Source) {
    let (label, source) = text.split_once('@').unwrap_or(("default", text));
    let source = if source == "prompt" {
        Source::Prompt
    } else {
        Source::File(PathBuf::from(source))
    };
    (label.to_string(), source)
}

/// `unfrackpath`: `~` expanded, made absolute, `.` and `..` folded away.
fn absolute(path: &Path) -> PathBuf {
    let mut full = path.to_path_buf();
    if let (Ok(rest), Some(home)) = (path.strip_prefix("~"), std::env::var_os("HOME")) {
        full = Path::new(&home).join(rest);
    }
    let full = std::path::absolute(&full).unwrap_or(full);
    let mut out = PathBuf::new();
    for part in full.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        // SAFETY: `access` reads a NUL-terminated path and nothing else.
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0)
}

/// Controllers run on Unix; elsewhere every password file is read as text.
#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

/// The exit status as the reference prints it: the code, or minus the signal.
fn exit_number(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

fn file_secret(label: &str, path: &Path) -> Result<Vec<u8>, LoadError> {
    let path = absolute(path);
    let getting = |message: String| LoadError {
        warning: format!("Error getting vault password file ({label}): {message}"),
        message,
    };
    let loading = |message: String| LoadError {
        warning: format!("Error in vault password file loading ({label}): {message}"),
        message,
    };
    if !path.exists() {
        return Err(getting(format!(
            "The vault password file {} was not found",
            path.display()
        )));
    }
    if path.is_dir() {
        return Err(getting(format!(
            "The vault password file provided '{}' can not be a directory",
            path.display()
        )));
    }
    let (secret, kind) = if is_executable(&path) {
        let out = std::process::Command::new(&path)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .output()
            .map_err(|e| {
                loading(format!(
                    "Problem running vault password script {} ({e}). If this is not a script, remove the executable bit from the file.",
                    path.display()
                ))
            })?;
        if !out.status.success() {
            let rc = exit_number(out.status);
            return Err(loading(format!(
                "Vault password script {} returned non-zero ({rc}): None",
                path.display()
            )));
        }
        (strip_crlf(&out.stdout).to_vec(), "script")
    } else {
        let bytes = std::fs::read(&path).map_err(|_| {
            loading(format!(
                "Could not read vault password file '{}'.",
                path.display()
            ))
        })?;
        (bytes.trim_ascii().to_vec(), "file")
    };
    if secret.is_empty() {
        return Err(loading(format!(
            "Invalid vault password was provided from {kind} ({})",
            path.display()
        )));
    }
    Ok(secret)
}

/// What was typed, surrounding whitespace removed; `None` when nothing is left.
fn typed_secret(typed: &[u8]) -> Option<Vec<u8>> {
    Some(typed.trim_ascii().to_vec()).filter(|s| !s.is_empty())
}

fn prompt_secret(label: &str) -> Result<Vec<u8>, LoadError> {
    let text = if label == "default" {
        "Vault password: ".to_string()
    } else {
        format!("Vault password ({label}): ")
    };
    let fail = |message: String| LoadError {
        warning: format!("Error in vault password prompt ({label}): {message}"),
        message,
    };
    match crate::prompt::hidden_line(&text) {
        Ok(Some(typed)) => {
            typed_secret(&typed).ok_or_else(|| fail("Invalid vault password was provided".into()))
        }
        Ok(None) => Err(fail(format!("EOFError (ctrl-d) on prompt for ({label})"))),
        Err(e) => Err(fail(e.to_string())),
    }
}

/// What the command line and the configuration ask for, in the reference's order: identity list
/// from the configuration, `--vault-id` entries, then the password files, then the prompt.
#[derive(Debug, Default)]
pub struct Request {
    pub ask: bool,
    pub password_files: Vec<PathBuf>,
    pub ids: Vec<String>,
    pub config_file: Option<PathBuf>,
    pub config_ids: Vec<String>,
}

pub fn load(request: &Request) -> Result<Secrets, LoadError> {
    let mut specs: Vec<(String, Source)> = request
        .config_ids
        .iter()
        .chain(&request.ids)
        .map(|s| spec(s))
        .collect();
    specs.extend(
        request
            .password_files
            .iter()
            .chain(&request.config_file)
            .map(|p| ("default".to_string(), Source::File(p.clone()))),
    );
    if request.ask {
        specs.push(("default".to_string(), Source::Prompt));
    }
    let mut secrets = Vec::new();
    for (label, source) in specs {
        secrets.push(match source {
            Source::Prompt => prompt_secret(&label)?,
            Source::File(path) => file_secret(&label, &path)?,
        });
    }
    Ok(Secrets(secrets))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/vault")
                .join(name),
        )
        .expect("fixture")
    }

    /// The vault text of a `name: !vault |` fixture, its indentation removed.
    fn yaml_value(name: &str) -> Vec<u8> {
        let text = String::from_utf8(fixture(name)).unwrap();
        let lines: Vec<&str> = text.lines().skip(1).map(str::trim).collect();
        format!("{}\n", lines.join("\n")).into_bytes()
    }

    fn secrets(names: &[&str]) -> Secrets {
        Secrets::new(
            names
                .iter()
                .map(|n| fixture(n).trim_ascii().to_vec())
                .collect(),
        )
    }

    #[rstest]
    #[case::file_11("f11.txt", &["pw1"], b"hello")]
    #[case::file_11_pw2_first("fd.txt", &["pw2", "pw1"], b"hello")]
    #[case::file_12_label_ignored("f2.txt", &["pw1", "pw2"], b"hello")]
    fn files_decrypt_byte_for_byte(#[case] name: &str, #[case] pw: &[&str], #[case] want: &[u8]) {
        assert_eq!(decrypt(&fixture(name), &secrets(pw)).unwrap(), want);
    }

    #[rstest]
    #[case::var_11("v11.yml", b"secret11")]
    #[case::var_12("v12.yml", b"secret12")]
    fn yaml_values_decrypt(#[case] name: &str, #[case] want: &[u8]) {
        assert_eq!(
            decrypt(&yaml_value(name), &secrets(&["pw1", "pw2"])).unwrap(),
            want
        );
    }

    #[rstest]
    #[case::wrong_password("err-f.yml", &["pw2"], "Decryption failed (no vault secrets were found that could decrypt)")]
    #[case::no_password("err-f.yml", &[], "Attempting to decrypt but no vault secrets found")]
    #[case::corrupt("err-fcorrupt.yml", &["pw1"], "Vault format unhexlify error: Non-hexadecimal digit found")]
    #[case::bad_version("err-fh.yml", &["pw1"], "Vault vaulttext format error: not enough values to unpack (expected 3, got 1)")]
    #[case::not_hex("err-fz.yml", &["pw1"], "Vault format unhexlify error: Non-hexadecimal digit found")]
    #[case::plain("err-h.txt", &["pw1"], "Input is not vault encrypted data.")]
    fn bad_data_is_refused_in_the_references_words(
        #[case] name: &str,
        #[case] pw: &[&str],
        #[case] want: &str,
    ) {
        let err = decrypt(&fixture(name), &secrets(pw)).unwrap_err();
        assert_eq!(err.0, want);
    }

    #[rstest]
    #[case::v11("f11.txt", None, "pw1")]
    #[case::default_id_is_11("f11.txt", Some("default"), "pw1")]
    #[case::v12("f2.txt", Some("b"), "pw2")]
    fn encrypting_with_the_fixtures_salt_gives_the_fixture(
        #[case] name: &str,
        #[case] id: Option<&str>,
        #[case] pw: &str,
    ) {
        let data = fixture(name);
        let salt = parts(&envelope(&data).unwrap().body).unwrap().salt;
        let salt: [u8; 32] = salt.try_into().unwrap();
        let got = encrypt_with_salt(b"hello", fixture(pw).trim_ascii(), id, &salt);
        assert_eq!(got.as_bytes(), &data[..]);
    }

    #[test]
    fn a_fresh_encryption_decrypts_and_wraps_at_80() {
        let text = encrypt(b"abc", b"pw", Some("dev")).unwrap();
        assert!(text.lines().skip(1).all(|l| l.len() <= WIDTH));
        assert!(text.ends_with('\n'));
        let got = decrypt(text.as_bytes(), &Secrets::new(vec![b"pw".to_vec()])).unwrap();
        assert_eq!(got, b"abc");
    }

    #[test]
    fn secrets_do_not_show_in_debug() {
        assert_eq!(
            format!("{:?}", Secrets::new(vec![b"hunter2".to_vec()])),
            "Secrets(1 redacted)"
        );
    }

    #[rstest]
    #[case::bare("pw1", "default")]
    #[case::labelled("dev@pw1", "dev")]
    fn a_spec_names_its_label(#[case] text: &str, #[case] label: &str) {
        assert_eq!(spec(text).0, label);
    }

    #[test]
    fn prompt_is_the_only_source_that_is_not_a_file() {
        assert_eq!(spec("dev@prompt").1, Source::Prompt);
        assert_eq!(spec("prompt").1, Source::Prompt);
        assert_eq!(spec("a@b").1, Source::File("b".into()));
    }

    #[rstest]
    #[case::plain(b"pw1", Some(&b"pw1"[..]))]
    #[case::spaced(b"  pw1 	", Some(&b"pw1"[..]))]
    #[case::empty(b"", None)]
    #[case::blank(b" 	 ", None)]
    fn a_typed_password_is_stripped_and_not_empty(
        #[case] typed: &[u8],
        #[case] want: Option<&[u8]>,
    ) {
        assert_eq!(typed_secret(typed).as_deref(), want);
    }

    #[test]
    fn a_later_install_replaces_the_secrets() {
        install(Secrets::new(vec![b"a".to_vec()]));
        install(Secrets::new(vec![b"b".to_vec()]));
        assert_eq!(run_secrets().first(), Some(&b"b"[..]));
    }

    #[test]
    fn script_output_loses_cr_and_lf_at_both_ends() {
        assert_eq!(strip_crlf(b"\r\n pw \r\n"), b" pw ");
        assert_eq!(strip_crlf(b"\n\r"), b"");
    }

    #[cfg(unix)]
    mod files {
        use super::*;
        use std::os::unix::fs::PermissionsExt as _;

        fn write(dir: &Path, name: &str, body: &[u8], mode: u32) -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        }

        fn request(path: &Path, id: &str) -> Request {
            Request {
                ids: vec![format!("{id}@{}", path.display())],
                ..Request::default()
            }
        }

        #[rstest]
        #[case::crlf(b"pw1\r\n", 0o600, b"pw1")]
        #[case::newline(b"pw1\n", 0o600, b"pw1")]
        #[case::script(b"#!/bin/sh\nprintf 'pw1\\r\\n'\n", 0o700, b"pw1")]
        fn a_password_file_or_script_gives_its_text(
            #[case] body: &[u8],
            #[case] mode: u32,
            #[case] want: &[u8],
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = write(dir.path(), "pw", body, mode);
            let got = load(&request(&path, "dev")).unwrap();
            assert_eq!(got.first(), Some(want));
        }

        #[test]
        fn an_unreadable_file_is_named_in_quotes() {
            let dir = tempfile::tempdir().unwrap();
            let path = write(dir.path(), "pw", b"x", 0o000);
            if std::fs::read(&path).is_ok() {
                return; // running as root: nothing is unreadable
            }
            let err = load(&request(&path, "d")).unwrap_err();
            assert_eq!(
                err.message,
                format!("Could not read vault password file '{}'.", path.display())
            );
        }

        #[test]
        fn a_script_gets_no_arguments_and_a_failure_is_named() {
            let dir = tempfile::tempdir().unwrap();
            let ok = write(
                dir.path(),
                "ok.sh",
                b"#!/bin/sh\n[ $# -eq 0 ] && echo pw1\n",
                0o700,
            );
            assert_eq!(
                load(&request(&ok, "dev")).unwrap().first(),
                Some(&b"pw1"[..])
            );
            let bad = write(dir.path(), "bad.sh", b"#!/bin/sh\nexit 3\n", 0o700);
            let err = load(&request(&bad, "x")).unwrap_err();
            let shown = bad.display();
            assert_eq!(
                err.message,
                format!("Vault password script {shown} returned non-zero (3): None")
            );
            assert_eq!(
                err.warning,
                format!("Error in vault password file loading (x): {}", err.message)
            );
        }

        #[test]
        fn missing_directory_and_empty_files_are_refused() {
            let dir = tempfile::tempdir().unwrap();
            let shown = dir.path().display();
            let missing = load(&request(&dir.path().join("nope"), "d")).unwrap_err();
            assert_eq!(
                missing.message,
                format!("The vault password file {shown}/nope was not found")
            );
            assert!(
                missing
                    .warning
                    .starts_with("Error getting vault password file (d): ")
            );
            let folder = load(&request(dir.path(), "d")).unwrap_err();
            assert_eq!(
                folder.message,
                format!("The vault password file provided '{shown}' can not be a directory")
            );
            let empty = write(dir.path(), "empty", b"\n", 0o600);
            let err = load(&request(&empty, "d")).unwrap_err();
            assert_eq!(
                err.message,
                format!("Invalid vault password was provided from file ({shown}/empty)")
            );
        }

        #[test]
        fn every_source_is_collected_config_first() {
            let dir = tempfile::tempdir().unwrap();
            let a = write(dir.path(), "a", b"one\n", 0o600);
            let b = write(dir.path(), "b", b"two\n", 0o600);
            let c = write(dir.path(), "c", b"three\n", 0o600);
            let got = load(&Request {
                password_files: vec![b],
                ids: vec![format!("x@{}", a.display())],
                config_file: Some(c),
                ..Request::default()
            })
            .unwrap();
            assert_eq!(got.0, [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
        }
    }
}
