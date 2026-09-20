// SPDX-License-Identifier: GPL-3.0-or-later
//! The agent's cache of module payloads, addressed by the blake3 hash of the zip they hold.
//!
//! One rule holds the whole file together: a payload is the right payload because its bytes hash
//! to its name, never because a file of that name exists. `remote_tmp` is `/var/tmp` on many
//! hosts, the name of a payload is public - it is the hash of a zip built from a released
//! ansible-core - and what the agent answers `present: true` for is what it later runs, often
//! under `become`. So the bytes are read back and hashed on every path that can answer yes, the
//! cache directory is refused unless this agent owns it and nobody else can write it, and a name
//! that is not 64 hex characters never reaches the filesystem at all.

use std::fs;
use std::io::{self, Write};

use std::path::{Path, PathBuf};
use volant_protocol::{FromAgent, LogLevel, ToAgent};

/// Where the agent keeps payloads, and where it reads `remote_tmp` from.
///
/// `VOLANT_REMOTE_TMP` when the controller sets it; otherwise the agent's own path, which names
/// `remote_tmp` on the ssh transport. `std::env::temp_dir()` when neither answers - a directory
/// the user running the agent can always write.
///
/// **For task 3:** have the controller set `VOLANT_REMOTE_TMP`. It holds the configured
/// `remote_tmp` on both transports and the derivation below only covers one of them: the local
/// transport starts the agent from wherever it was installed, so the operator's configured value
/// is not consulted at all and the cache falls back to the temporary directory.
pub fn remote_tmp() -> String {
    if let Ok(dir) = std::env::var("VOLANT_REMOTE_TMP") {
        return dir;
    }
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(tmp_from_exe)
        .and_then(|dir| dir.to_str())
        .map_or_else(
            || std::env::temp_dir().to_string_lossy().into_owned(),
            str::to_string,
        )
}

/// `remote_tmp` read back from the agent's own path, and only where the ssh transport puts it:
/// `<remote_tmp>/volant-agent-<version>/volant-agent`.
///
/// Anywhere else, the grandparent is somebody else's directory rather than `remote_tmp` - a
/// system-wide install over the local transport would make it `/usr/local` - so the shape is
/// checked instead of assumed.
fn tmp_from_exe(exe: &Path) -> Option<&Path> {
    let cache = exe.parent()?;
    if !cache.file_name()?.to_str()?.starts_with("volant-agent-") {
        return None;
    }
    cache.parent()
}

/// The cache directory's name, which carries the effective uid.
///
/// Per user, because `remote_tmp` is shared: two operators on one host, or the same operator with
/// and without `become`, would otherwise meet in one directory that only the first of them can
/// write - and every run of the second would fail identically until somebody found and removed a
/// directory in `/var/tmp` they had no reason to look at.
fn dir(remote_tmp: &str) -> PathBuf {
    Path::new(remote_tmp).join(format!(
        "volant-blobs-{}-{}",
        env!("CARGO_PKG_VERSION"),
        euid()
    ))
}

/// The cache directory, created mode 0700 and refused unless it is this agent's alone.
///
/// Created with the mode rather than created and then chmod'ed: the second form leaves a window
/// where another user can drop a file in, and it also `EPERM`s against a directory somebody else
/// owns instead of saying so.
fn cache_dir(remote_tmp: &str) -> io::Result<PathBuf> {
    let dir = dir(remote_tmp);
    create_private(&dir)?;
    check_private(&dir)?;
    Ok(dir)
}

/// Where a payload of this hash sits, once the name is known to be one.
///
/// The name comes off the wire and is joined onto a path, so it is checked here rather than in
/// each of the three places that will open the result: `../../home/mallory/evil` is a blob name
/// no agent should be able to build a path from.
pub fn path(remote_tmp: &str, hash: &str) -> io::Result<PathBuf> {
    if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("'{hash}' is not a payload name: 64 lowercase hex characters"),
        ));
    }
    Ok(dir(remote_tmp).join(format!("{hash}.zip")))
}

/// Whether the cache holds this payload **and** its bytes still hash to its name.
///
/// The name alone is never enough, for the reason at the top of this file. Reading and hashing
/// the 631 KB union payload costs a fraction of a millisecond, once per run for each link, which
/// buys the only thing that makes `present: true` mean what the controller reads it to mean.
/// Bytes that do not match answer `false`, so the controller re-sends and [`store`] replaces
/// them: a cache poisoned by a power loss mid-rename heals on the next run instead of being
/// trusted forever.
pub fn holds(remote_tmp: &str, hash: &str) -> io::Result<bool> {
    let at = path(remote_tmp, hash)?;
    cache_dir(remote_tmp)?;
    matches_hash(&at, hash)
}

/// Answers a blob message, wherever in the conversation it arrives.
///
/// `serve` takes these between batches and `runner::is_cancelled` takes them during one, and
/// both have to answer: a controller that sent `put_blob` waits for a `BlobState`, so one
/// logged and dropped mid-batch left it waiting for a state that never came. `Ok(false)` says
/// the message was not a blob message and is still the caller's to handle.
pub fn answer<F>(remote_tmp: &str, msg: &ToAgent, send: &mut F) -> io::Result<bool>
where
    F: FnMut(&FromAgent) -> io::Result<()>,
{
    match msg {
        ToAgent::HasBlob { hash } => {
            let present = match holds(remote_tmp, hash) {
                Ok(present) => present,
                Err(err) => {
                    send(&FromAgent::Log {
                        level: LogLevel::Error,
                        message: format!("looking for payload {hash}: {err}"),
                    })?;
                    false
                }
            };
            send(&FromAgent::BlobState {
                hash: hash.clone(),
                present,
            })?;
        }
        // A refused payload is answered, never left silent: the controller waits for this state
        // before it sends the batch that needs the payload, and the log is the only place the
        // reason survives.
        ToAgent::PutBlob { hash, zip_b64 } => match store(remote_tmp, hash, zip_b64) {
            Ok(_) => send(&FromAgent::BlobState {
                hash: hash.clone(),
                present: true,
            })?,
            Err(err) => {
                send(&FromAgent::Log {
                    level: LogLevel::Error,
                    message: format!("storing payload {hash}: {err}"),
                })?;
                send(&FromAgent::BlobState {
                    hash: hash.clone(),
                    present: false,
                })?;
            }
        },
        _ => return Ok(false),
    }
    Ok(true)
}

/// Decodes, hashes, refuses a mismatch, then writes atomically under the hash.
///
/// The hash is checked on the decoded bytes **before** anything is written, so a mismatch never
/// leaves a file behind under any name, nor even the cache directory. The temporary name carries
/// the pid so two agents racing on one host cannot interleave into a file of exactly the right
/// length.
pub fn store(remote_tmp: &str, hash: &str, zip_b64: &str) -> io::Result<PathBuf> {
    store_with(free_bytes, remote_tmp, hash, zip_b64)
}

/// [`store`] with the free-space reader handed in, so a test can drive the guard's call site
/// rather than only the guard.
fn store_with(
    free: impl Fn(&Path) -> io::Result<u64>,
    remote_tmp: &str,
    hash: &str,
    zip_b64: &str,
) -> io::Result<PathBuf> {
    let zip = decode_b64(zip_b64).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let actual = blake3::hash(&zip).to_hex().to_string();
    if actual != hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload arrived as '{hash}' but its bytes hash to '{actual}'"),
        ));
    }
    let final_path = path(remote_tmp, hash)?;
    // Before the directory exists: `create_dir_all` is itself a write, and on a filesystem that
    // is genuinely full it would fail `ENOSPC` first and hide the two figures below.
    check_space(
        &free,
        nearest_existing(Path::new(remote_tmp)),
        zip.len() as u64,
    )?;
    let dir = cache_dir(remote_tmp)?;
    if matches_hash(&final_path, hash)? {
        return Ok(final_path);
    }
    let tmp = dir.join(format!("{hash}.tmp.{}", std::process::id()));
    let mut file = fs::File::create(&tmp)?;
    if let Err(err) = file.write_all(&zip).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    drop(file);
    if let Err(err) = fs::rename(&tmp, &final_path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    // The file's own `sync_all` above only promises its contents. Without this, a power loss
    // just after the rename can leave the directory entry pointing at a short or zero-filled
    // file, and `holds` is the only thing that would ever notice - one run too late, since the
    // controller has already been told the payload is there.
    sync_dir(&dir)?;
    Ok(final_path)
}

/// Whether the file at `at` reads back as the payload named `hash`. A file that is not there is
/// not a match; anything else the filesystem says is an error rather than a silent `false`.
fn matches_hash(at: &Path, hash: &str) -> io::Result<bool> {
    match fs::read(at) {
        Ok(zip) => Ok(blake3::hash(&zip).to_hex().to_string() == hash),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Refuses a payload the filesystem cannot hold, before a byte of it is written.
///
/// A short write noticed afterwards would already have spent the space, and the message an
/// operator gets would be `ENOSPC` on a temporary name rather than the two figures that say how
/// far short the host is.
fn check_space(free: impl Fn(&Path) -> io::Result<u64>, dir: &Path, need: u64) -> io::Result<()> {
    let free = free(dir)?;
    if free < need {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            format!(
                "payload needs {need} bytes, the filesystem holding {} has {free}",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// The nearest ancestor of `dir` that exists, for a question only an existing directory can be
/// asked. `remote_tmp` is `~/.ansible/tmp` by default and may not have been created yet.
fn nearest_existing(dir: &Path) -> &Path {
    dir.ancestors()
        .find(|ancestor| ancestor.exists())
        .unwrap_or(dir)
}

/// Creates the cache directory mode 0700, and says nothing when it is already there - the
/// ownership and mode of one that already exists is [`check_private`]'s answer to give.
#[cfg(unix)]
fn create_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let make = || fs::DirBuilder::new().mode(0o700).create(dir);
    match make() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent)?;
            }
            make()
        }
        other => other,
    }
    .or_else(|err| match err.kind() {
        io::ErrorKind::AlreadyExists => Ok(()),
        _ => Err(err),
    })
}

/// The agent runs on a managed unix host and nowhere else; the arms off unix only keep the crate
/// compiling on a developer's machine.
#[cfg(not(unix))]
fn create_private(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Refuses a cache this agent does not own outright.
///
/// Both halves matter. A directory another user owns cannot be trusted and must not be chmod'ed
/// into shape - that would be `EPERM` forever on a shared `remote_tmp`. A directory anyone else
/// can write is worse: they seed a file under a hash they computed offline, and the agent would
/// answer for a payload it never received.
#[cfg(unix)]
fn check_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::metadata(dir)?;
    let mode = meta.mode() & 0o7777;
    if meta.uid() != euid() || mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {} with mode {mode:o}; this agent runs as uid {} and will not \
                 trust a payload cache another user can write",
                dir.display(),
                meta.uid(),
                euid()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Makes the rename itself durable, not just the bytes it renames.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn euid() -> u32 {
    0
}

/// Bytes an unprivileged process can still write to the filesystem holding `dir`.
#[cfg(unix)]
fn free_bytes(dir: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated C string that outlives the call, and `stat` is a
    // writable, correctly aligned `statvfs` the call fills before it returns 0.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call above returned 0, so it initialised the structure.
    let stat = unsafe { stat.assume_init() };
    // Through `u128` because the two fields are as wide as a `u64` on the targets the agent is
    // built for, and their product is not.
    let free = u128::from(stat.f_bavail) * u128::from(stat.f_frsize);
    Ok(u64::try_from(free).unwrap_or(u64::MAX))
}

/// Nothing asks the filesystem off unix, where the agent never runs: the guard passes and the
/// write itself is left to report a full filesystem.
#[cfg(not(unix))]
fn free_bytes(_dir: &Path) -> io::Result<u64> {
    Ok(u64::MAX)
}

/// Standard base64 with padding, the form `modify_module` hands the zip back in.
///
/// Written here rather than pulled in: the agent is uploaded to every managed host, and one
/// decoder of thirty lines is cheaper than a dependency for a single call. Newlines are skipped
/// so a wrapped encoding decodes, and anything else is refused with the offset that broke it -
/// a payload half-decoded into something that happens to hash to nothing is not a failure an
/// operator could read.
pub(crate) fn decode_b64(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut padded = false;
    for (index, &byte) in text.as_bytes().iter().enumerate() {
        match byte {
            b'\n' | b'\r' => continue,
            b'=' => {
                padded = true;
                continue;
            }
            _ if padded => return Err(format!("base64 character after padding at offset {index}")),
            _ => {}
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(format!("invalid base64 character at offset {index}")),
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits == 24 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            acc = 0;
            bits = 0;
        }
    }
    match bits {
        0 => {}
        12 => out.push((acc >> 4) as u8),
        18 => {
            out.push((acc >> 10) as u8);
            out.push((acc >> 2) as u8);
        }
        _ => return Err("base64 ends in the middle of a byte".to_string()),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    /// A payload whose bytes do not hash to the name it arrived under never lands, and the error
    /// says both hashes so an operator can tell a corrupted transfer from a wrong name.
    ///
    /// What would make this red: the hash checked after the file is renamed into place, which
    /// leaves a poisoned cache entry that every later run reuses; or the check skipped when the
    /// file already exists, which is the same hole one run later.
    #[test]
    fn a_blob_whose_hash_does_not_match_is_refused_before_it_lands() {
        let dir = tempdir();
        let zip = b"not really a zip";
        let wrong = "0".repeat(64);
        let err = store(dir.path().to_str().unwrap(), &wrong, &b64(zip)).unwrap_err();
        assert!(err.to_string().contains(&wrong), "{err}");
        assert!(err.to_string().contains(&hash_of(zip)), "{err}");
        assert!(!path(dir.path().to_str().unwrap(), &wrong).unwrap().exists());
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            0,
            "not even a temporary file survives a mismatch"
        );
    }

    /// A payload that does match lands under its hash and is readable back byte for byte.
    #[test]
    fn a_matching_blob_lands_under_its_hash() {
        let dir = tempdir();
        let zip = b"pretend this is a zip";
        let h = hash_of(zip);
        let at = store(dir.path().to_str().unwrap(), &h, &b64(zip)).unwrap();
        assert_eq!(fs::read(&at).unwrap(), zip);
        assert_eq!(at, path(dir.path().to_str().unwrap(), &h).unwrap());
    }

    /// A file already sitting under a hash is answered for only once its bytes are read back and
    /// hashed, and one that does not match is replaced rather than handed out.
    ///
    /// What would make this red: `holds` answering from the name alone. `remote_tmp` is
    /// world-writable on most hosts and the name of a payload is public - it is the hash of a zip
    /// built from a released ansible-core - so a local user who plants a file under it would have
    /// their zip answered for, never re-sent, and run as whoever the agent runs as.
    #[test]
    fn a_planted_blob_is_not_trusted_on_its_name() {
        let root = tempdir();
        let remote_tmp = root.path().to_str().unwrap();
        let zip = b"pretend this is a zip";
        let h = hash_of(zip);
        let at = path(remote_tmp, &h).unwrap();
        // The cache itself is this agent's own, so what is under test is the file and not the
        // directory: a power loss between the rename and the next run leaves exactly this.
        create_private(&dir(remote_tmp)).unwrap();
        fs::write(&at, b"a zip of the planter's choosing").unwrap();

        assert!(
            !holds(remote_tmp, &h).unwrap(),
            "the bytes are not the payload"
        );

        let landed = store(remote_tmp, &h, &b64(zip)).unwrap();
        assert_eq!(landed, at);
        assert_eq!(
            fs::read(&at).unwrap(),
            zip,
            "the planted bytes are replaced"
        );
        assert!(holds(remote_tmp, &h).unwrap());
    }

    /// A payload already in the cache and still matching its name is handed back where it is, not
    /// written again.
    ///
    /// What would make this red: the file rewritten on every `put_blob`, which spends the
    /// transfer this cache exists to avoid. Proved on the inode, because a rewrite goes through a
    /// new temporary file and a rename.
    #[cfg(unix)]
    #[test]
    fn a_matching_blob_already_present_is_not_written_again() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempdir();
        let remote_tmp = dir.path().to_str().unwrap();
        let zip = b"pretend this is a zip";
        let h = hash_of(zip);
        let at = store(remote_tmp, &h, &b64(zip)).unwrap();
        let first = fs::metadata(&at).unwrap().ino();
        let again = store(remote_tmp, &h, &b64(zip)).unwrap();
        assert_eq!(again, at);
        assert_eq!(
            fs::metadata(&at).unwrap().ino(),
            first,
            "the file was replaced"
        );
    }

    /// A cache directory anyone but this agent can write is refused, with its mode in the
    /// message, rather than chmod'ed into shape.
    ///
    /// What would make this red: `create_dir_all` followed by an unconditional chmod. Against a
    /// directory the agent owns that hides the planting window this test stands in for; against
    /// one another user owns it returns `EPERM` on every run from then on, and two operators
    /// sharing a `/var/tmp` break each other permanently.
    #[cfg(unix)]
    #[test]
    fn a_cache_directory_others_can_write_is_refused() {
        use std::os::unix::fs::DirBuilderExt;

        let root = tempdir();
        let remote_tmp = root.path().to_str().unwrap();
        fs::DirBuilder::new()
            .mode(0o755)
            .create(dir(remote_tmp))
            .unwrap();
        let zip = b"pretend this is a zip";
        let err = store(remote_tmp, &hash_of(zip), &b64(zip)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(err.to_string().contains("755"), "{err}");
        assert!(holds(remote_tmp, &hash_of(zip)).is_err());
    }

    /// The cache is closed to everyone but its owner: `remote_tmp` is world-writable on most
    /// hosts, and a payload another user could drop in would be answered for on its name alone.
    ///
    /// The umask is deliberately loose, so the mode has to come from the directory being created
    /// with it rather than from whatever the developer's shell happened to set.
    #[cfg(unix)]
    #[test]
    fn the_cache_directory_is_private_to_the_agent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir();
        let zip = b"pretend this is a zip";
        // SAFETY: nextest runs each test in its own process, so this reaches no other test.
        let previous = unsafe { libc::umask(0o022) };
        let stored = store(dir.path().to_str().unwrap(), &hash_of(zip), &b64(zip));
        // SAFETY: as above, and the value put back is the one just taken.
        unsafe { libc::umask(previous) };
        stored.unwrap();
        let mode = fs::metadata(super::dir(dir.path().to_str().unwrap()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }

    /// A blob name that is not 64 lowercase hex characters never reaches the filesystem.
    ///
    /// What would make this red: `path` joining whatever arrives. Task 3 opens what `path`
    /// returns, so a `Task.payload.blob` of `../../home/mallory/evil` would read and run a file
    /// from outside the cache.
    #[test]
    fn a_blob_name_that_is_not_a_hash_is_refused() {
        let dir = tempdir();
        let remote_tmp = dir.path().to_str().unwrap();
        for name in [
            "../volant-agent-0.1.0/volant-agent",
            "",
            &"0".repeat(63),
            &"0".repeat(65),
            &"F".repeat(64),
        ] {
            assert_eq!(
                path(remote_tmp, name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{name}"
            );
            assert!(holds(remote_tmp, name).is_err(), "{name}");
            assert!(store(remote_tmp, name, &b64(b"zip")).is_err(), "{name}");
        }
    }

    /// Base64 the agent cannot decode is refused as invalid data, before anything is written and
    /// with the offset that broke it.
    ///
    /// What would make this red: a decoder that skips what it does not recognise, which turns a
    /// truncated transfer into bytes that hash to something and land under a name no later run
    /// can tell from a good one.
    #[test]
    fn a_payload_that_is_not_base64_is_refused_before_it_lands() {
        let dir = tempdir();
        let err = store(dir.path().to_str().unwrap(), &"0".repeat(64), "UEsD!BA==").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("offset 4"), "{err}");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// The decoder is checked against a known encoding and on every padding length, because one
    /// byte wrong turns every payload the controller sends into a hash mismatch.
    #[test]
    fn base64_decodes_the_zip_magic_and_every_padding_length() {
        assert_eq!(decode_b64("UEsDBA==").unwrap(), b"PK\x03\x04");
        assert!(decode_b64("").unwrap().is_empty());
        for len in 0..=9usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(decode_b64(&b64(&bytes)).unwrap(), bytes, "{len} bytes");
        }
        assert!(
            decode_b64("A").is_err(),
            "one character cannot end a payload"
        );
        assert!(
            decode_b64("UEsDBA==A").is_err(),
            "nothing follows the padding"
        );
    }

    /// A payload larger than what the filesystem has left is refused with both figures and the
    /// directory in the message.
    ///
    /// What would make this red: the space checked after the write, where the operator gets
    /// `ENOSPC` on a temporary name and no idea how far short the host is.
    #[cfg(unix)]
    #[test]
    fn a_payload_larger_than_the_filesystem_is_refused_with_both_figures() {
        let dir = tempdir();
        let free = free_bytes(dir.path()).unwrap();
        let err = check_space(free_bytes, dir.path(), u64::MAX).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        assert!(err.to_string().contains(&u64::MAX.to_string()), "{err}");
        assert!(err.to_string().contains(&free.to_string()), "{err}");
        assert!(
            err.to_string().contains(&dir.path().display().to_string()),
            "{err}"
        );
        check_space(free_bytes, dir.path(), 1).expect("one byte fits");
    }

    /// `store` asks about free space before it writes, and refuses without leaving a temporary
    /// file behind.
    ///
    /// What would make this red: the call to the guard dropped while the guard itself stays in
    /// the file, which every other test in here would let through.
    #[test]
    fn store_refuses_a_payload_the_filesystem_cannot_hold() {
        let dir = tempdir();
        let remote_tmp = dir.path().to_str().unwrap();
        let zip = b"pretend this is a zip";
        let err = store_with(|_| Ok(0), remote_tmp, &hash_of(zip), &b64(zip)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull, "{err}");
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            0,
            "the cache is not even created for a payload that cannot fit"
        );
    }

    /// A `remote_tmp` the cache cannot be created under fails with what the filesystem said, and
    /// that is what `serve` turns into a log and `BlobState { present: false }`.
    #[cfg(unix)]
    #[test]
    fn a_remote_tmp_the_cache_cannot_be_created_under_fails() {
        let dir = tempdir();
        let occupied = dir.path().join("occupied");
        fs::write(&occupied, b"").unwrap();
        let zip = b"pretend this is a zip";
        let err = store(occupied.to_str().unwrap(), &hash_of(zip), &b64(zip)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{err}");
    }

    /// The cache root is read back from the agent's own path only where the ssh transport puts
    /// it. Anywhere else the agent was started from names somebody else's directory.
    ///
    /// What would make this red: taking the grandparent whatever it is. A system-wide install
    /// runs `/usr/local/bin/volant-agent` over the local transport, and the cache would be
    /// `/usr/local/volant-blobs-...`, which an ordinary user cannot create and which a run under
    /// `sudo` would leave behind owned by root.
    #[test]
    fn the_cache_root_comes_from_the_agents_path_only_where_the_transport_puts_it() {
        assert_eq!(
            tmp_from_exe(Path::new("/var/tmp/volant-agent-0.1.0/volant-agent")),
            Some(Path::new("/var/tmp"))
        );
        assert_eq!(tmp_from_exe(Path::new("/usr/local/bin/volant-agent")), None);
        assert_eq!(tmp_from_exe(Path::new("volant-agent")), None);
    }

    fn hash_of(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    /// Standard base64 with padding, the form the controller sends in.
    fn b64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut block = [0u8; 3];
            block[..chunk.len()].copy_from_slice(chunk);
            let bits = u32::from_be_bytes([0, block[0], block[1], block[2]]);
            for slot in 0..4 {
                if slot <= chunk.len() {
                    out.push(ALPHABET[(bits >> (18 - 6 * slot)) as usize & 0x3f] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// A directory of this process's own, removed when the test ends. The agent depends on
    /// `libc`, `serde_json`, `shlex`, `volant-protocol` and `blake3`, and a temporary-directory
    /// crate for a handful of tests is not worth carrying onto every managed host.
    struct TempDir(PathBuf);

    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "volant-blobs-test-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("the test directory is created");
        TempDir(dir)
    }
}
