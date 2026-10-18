// SPDX-License-Identifier: GPL-3.0-or-later
//! The vault wire format: envelope, key derivation, and decrypting and encrypting.

use std::collections::HashMap;
use std::fmt;
use std::sync::{LazyLock, Mutex};

use aes::Aes256;
use ctr::Ctr128BE;
use ctr::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::{Secrets, VaultError, is_encrypted};

const CIPHER: &str = "AES256";
const ITERATIONS: u32 = 10_000;
pub(super) const WIDTH: usize = 80;

type Aes256Ctr = Ctr128BE<Aes256>;

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
pub(super) fn strip_crlf(bytes: &[u8]) -> &[u8] {
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
pub(super) struct Envelope {
    cipher: String,
    /// The outer hex decoded: `hex(salt) "\n" hex(hmac) "\n" hex(ciphertext)`.
    pub(super) body: Vec<u8>,
}

pub(super) fn envelope(data: &[u8]) -> Result<Envelope, VaultError> {
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

pub(super) struct Parts {
    pub(super) salt: Vec<u8>,
    mac: Vec<u8>,
    ciphertext: Vec<u8>,
}

/// Salt, HMAC and ciphertext of an envelope's body.
pub(super) fn parts(body: &[u8]) -> Result<Parts, VaultError> {
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

type Derived = ([u8; 32], [u8; 32], [u8; 16]);
type Cache = HashMap<(Vec<u8>, Vec<u8>), Derived>;

/// PBKDF2-HMAC-SHA256, 10000 rounds, 80 bytes: AES key, HMAC key, counter start.
///
/// Kept per (secret, salt) for the life of the process: the same vaulted file is read again by
/// every task and every host that names it, and each derivation is 10000 rounds. The cache holds
/// nothing the secrets themselves do not already.
fn keys(secret: &[u8], salt: &[u8]) -> ([u8; 32], [u8; 32], [u8; 16]) {
    static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    let found = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(secret.to_vec(), salt.to_vec()))
        .copied();
    if let Some(derived) = found {
        return derived;
    }
    let derived = derive(secret, salt);
    CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((secret.to_vec(), salt.to_vec()), derived);
    derived
}

fn derive(secret: &[u8], salt: &[u8]) -> ([u8; 32], [u8; 32], [u8; 16]) {
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

pub(super) fn encrypt_with_salt(
    plain: &[u8],
    secret: &[u8],
    id: Option<&str>,
    salt: &[u8; 32],
) -> String {
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
