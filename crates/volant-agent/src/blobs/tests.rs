// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use crate::testing::scratch;

use volant_protocol::encoding::deflate;

/// A payload whose bytes do not hash to the name it arrived under never lands, and the error
/// says both hashes so an operator can tell a corrupted transfer from a wrong name.
#[test]
fn a_blob_whose_hash_does_not_match_is_refused_before_it_lands() {
    let dir = scratch();
    let zip = b"not really a zip";
    let wrong = "0".repeat(64);
    let err = store(dir.path().to_str().unwrap(), &wrong, zip, false).unwrap_err();
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
    let dir = scratch();
    let zip = b"pretend this is a zip";
    let h = hash_of(zip);
    let at = store(dir.path().to_str().unwrap(), &h, zip, false).unwrap();
    assert_eq!(fs::read(&at).unwrap(), zip);
    assert_eq!(at, path(dir.path().to_str().unwrap(), &h).unwrap());
}

/// A file already sitting under a hash is answered for only once its bytes are read back and
/// hashed, and one that does not match is replaced rather than handed out.
#[test]
fn a_planted_blob_is_not_trusted_on_its_name() {
    let root = scratch();
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

    let landed = store(remote_tmp, &h, zip, false).unwrap();
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
#[cfg(unix)]
#[test]
fn a_matching_blob_already_present_is_not_written_again() {
    use std::os::unix::fs::MetadataExt;

    let dir = scratch();
    let remote_tmp = dir.path().to_str().unwrap();
    let zip = b"pretend this is a zip";
    let h = hash_of(zip);
    let at = store(remote_tmp, &h, zip, false).unwrap();
    let first = fs::metadata(&at).unwrap().ino();
    let again = store(remote_tmp, &h, zip, false).unwrap();
    assert_eq!(again, at);
    assert_eq!(
        fs::metadata(&at).unwrap().ino(),
        first,
        "the file was replaced"
    );
}

/// A cache directory anyone but this agent can write is refused, with its mode in the message,
/// rather than chmod'ed into shape.
#[cfg(unix)]
#[test]
fn a_cache_directory_others_can_write_is_refused() {
    use std::os::unix::fs::DirBuilderExt;

    let root = scratch();
    let remote_tmp = root.path().to_str().unwrap();
    fs::DirBuilder::new()
        .mode(0o755)
        .create(dir(remote_tmp))
        .unwrap();
    let zip = b"pretend this is a zip";
    let err = store(remote_tmp, &hash_of(zip), zip, false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    assert!(err.to_string().contains("755"), "{err}");
    assert!(holds(remote_tmp, &hash_of(zip)).is_err());
}

/// The cache is closed to everyone but its owner: `remote_tmp` is world-writable on most hosts,
/// and a payload another user could drop in would be answered for on its name alone.
#[cfg(unix)]
#[test]
fn the_cache_directory_is_private_to_the_agent() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch();
    let zip = b"pretend this is a zip";
    // SAFETY: nextest runs each test in its own process, so this reaches no other test.
    let previous = unsafe { libc::umask(0o022) };
    let stored = store(dir.path().to_str().unwrap(), &hash_of(zip), zip, false);
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
#[test]
fn a_blob_name_that_is_not_a_hash_is_refused() {
    let dir = scratch();
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
        assert!(store(remote_tmp, name, b"zip", false).is_err(), "{name}");
    }
}

/// A raw frame is the blob only at exactly the announced length, and a deflated one only when
/// it inflates to exactly that length.
#[test]
fn a_blob_is_decoded_only_at_its_announced_length() {
    let zip = b"PK\x03\x04 and the rest of the zip".repeat(50);
    let len = zip.len() as u64;
    assert_eq!(&*decoded(len, BlobEncoding::Raw, &zip).unwrap(), &zip[..]);
    let packed = deflate(&zip);
    assert_eq!(
        &*decoded(len, BlobEncoding::Deflate, &packed).unwrap(),
        &zip[..]
    );

    let short = decoded(len, BlobEncoding::Raw, &zip[..zip.len() - 1]).unwrap_err();
    assert!(short.to_string().contains("where"), "{short}");
    let fewer = decoded(len + 1, BlobEncoding::Deflate, &packed).unwrap_err();
    assert!(fewer.to_string().contains("where"), "{fewer}");
    let cut = decoded(len, BlobEncoding::Deflate, &packed[..packed.len() / 2]).unwrap_err();
    assert!(cut.to_string().contains("does not inflate"), "{cut}");
    let past = decoded(BLOB_LIMIT as u64 + 1, BlobEncoding::Raw, b"").unwrap_err();
    assert!(past.to_string().contains("byte limit"), "{past}");
}

/// The frame after a `put_blob` is read as bytes, whatever they look like, and the message
/// after it is read as a message again: the stream stays in step through a refused frame.
#[test]
fn a_blob_frame_is_read_as_bytes_and_the_stream_stays_in_step() {
    use volant_protocol::frame::write_frame;
    let put = |len: u64| ToAgent::PutBlob {
        hash: "ab".into(),
        len,
        encoding: BlobEncoding::Raw,
        staged: false,
    };
    let json = |msg: &ToAgent| serde_json::to_vec(msg).unwrap();
    let has = ToAgent::HasBlob { hash: "ab".into() };
    let mut wire = Vec::new();
    write_frame(&mut wire, &json(&put(24))).unwrap();
    write_frame(&mut wire, br#"{"type":"cancel","id":1}"#).unwrap();
    write_frame(&mut wire, &json(&put(2))).unwrap();
    write_frame(&mut wire, b"longer than two").unwrap();
    write_frame(&mut wire, &json(&has)).unwrap();
    let mut input = io::Cursor::new(wire);
    let mut next = || read_incoming(&mut input).unwrap().unwrap().unwrap();

    let (msg, frame) = next();
    assert_eq!(msg, put(24));
    assert_eq!(frame.unwrap(), br#"{"type":"cancel","id":1}"#);
    let (msg, frame) = next();
    assert_eq!(msg, put(2));
    let why = frame.unwrap_err();
    assert!(why.contains("longer than the 2 bytes"), "{why}");
    assert_eq!(next(), (has, Ok(Vec::new())));
    assert!(read_incoming(&mut input).unwrap().is_none());
}

/// A `put_blob` that does not read has its frame read through and is answered as a refused
/// blob. A `len` past the limit is refused without the frame being held.
#[test]
fn a_put_blob_that_does_not_read_never_leaves_its_frame_behind() {
    use volant_protocol::frame::write_frame;
    let mut wire = Vec::new();
    write_frame(
        &mut wire,
        br#"{"type":"put_blob","hash":"ab","len":24,"encoding":"zstd","staged":true}"#,
    )
    .unwrap();
    write_frame(&mut wire, br#"{"type":"cancel","id":1}"#).unwrap();
    let past = ToAgent::PutBlob {
        hash: "cd".into(),
        len: BLOB_LIMIT as u64 + 1,
        encoding: BlobEncoding::Raw,
        staged: false,
    };
    write_frame(&mut wire, &serde_json::to_vec(&past).unwrap()).unwrap();
    write_frame(&mut wire, br#"{"type":"cancel","id":2}"#).unwrap();
    write_frame(&mut wire, br#"{"type":"has_blob","hash":"ab"}"#).unwrap();
    let mut input = io::Cursor::new(wire);
    let mut next = || read_incoming(&mut input).unwrap().unwrap().unwrap();

    let (msg, frame) = next();
    assert_eq!(
        msg,
        ToAgent::PutBlob {
            hash: "ab".into(),
            len: 0,
            encoding: BlobEncoding::Raw,
            staged: true
        }
    );
    let why = frame.unwrap_err();
    assert!(why.contains("zstd"), "{why}");
    let (msg, frame) = next();
    assert_eq!(msg, past);
    let why = frame.unwrap_err();
    assert!(why.contains("byte limit"), "{why}");
    assert_eq!(next().0, ToAgent::HasBlob { hash: "ab".into() });
}

/// A controller gone mid-blob is a broken stream, never a blob of the bytes that came.
#[test]
fn a_blob_frame_cut_by_the_end_of_the_stream_is_an_error() {
    let put = ToAgent::PutBlob {
        hash: "ab".into(),
        len: 4,
        encoding: BlobEncoding::Raw,
        staged: false,
    };
    let mut wire = Vec::new();
    volant_protocol::frame::write_frame(&mut wire, &serde_json::to_vec(&put).unwrap()).unwrap();
    wire.extend_from_slice(&[0, 0, 0, 4, b'P', b'K']);
    let err = read_incoming(io::Cursor::new(wire)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
}

/// A payload larger than what the filesystem has left is refused with both figures and the
/// directory in the message.
#[cfg(unix)]
#[test]
fn a_payload_larger_than_the_filesystem_is_refused_with_both_figures() {
    let dir = scratch();
    // The figure the check itself read: the space left moves while other tests write.
    let seen = std::cell::Cell::new(None);
    let err = check_space(
        |d: &Path| {
            let free = free_bytes(d)?;
            seen.set(Some(free));
            Ok(free)
        },
        dir.path(),
        u64::MAX,
    )
    .unwrap_err();
    let free = seen.get().expect("the check read the free space");
    assert_eq!(err.kind(), io::ErrorKind::StorageFull);
    assert!(err.to_string().contains(&u64::MAX.to_string()), "{err}");
    assert!(err.to_string().contains(&free.to_string()), "{err}");
    assert!(
        err.to_string().contains(&dir.path().display().to_string()),
        "{err}"
    );
    check_space(free_bytes, dir.path(), 1).expect("one byte fits");
}

/// `store` asks about free space before it writes, and refuses without leaving a temporary file
/// behind.
#[test]
fn store_refuses_a_payload_the_filesystem_cannot_hold() {
    let dir = scratch();
    let remote_tmp = dir.path().to_str().unwrap();
    let zip = b"pretend this is a zip";
    let err = store_with(|_| Ok(0), remote_tmp, &hash_of(zip), zip, false).unwrap_err();
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
    let dir = scratch();
    let occupied = dir.path().join("occupied");
    fs::write(&occupied, b"").unwrap();
    let zip = b"pretend this is a zip";
    let err = store(occupied.to_str().unwrap(), &hash_of(zip), zip, false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{err}");
}

/// The cache root is read back from the agent's own path only where the ssh transport puts it.
/// Anywhere else the agent was started from names somebody else's directory.
#[test]
fn the_cache_root_comes_from_the_agents_path_only_where_the_transport_puts_it() {
    assert_eq!(
        tmp_from_exe(Path::new("/var/tmp/volant-agent-0.1.0/volant-agent")),
        Some(Path::new("/var/tmp"))
    );
    assert_eq!(tmp_from_exe(Path::new("/usr/local/bin/volant-agent")), None);
    assert_eq!(tmp_from_exe(Path::new("volant-agent")), None);
}

/// A host name becomes one path component with no `-` in it, so the sweep's last `-` is always
/// the one before the pid.
#[test]
fn a_host_name_becomes_one_safe_component() {
    assert_eq!(host_component(b"web-1.example_org"), "web_1.example_org");
    assert_eq!(host_component(b"a/../b c"), "a_.._b_c");
    assert_eq!(host_component(&[b'x'; 300]).len(), 64);
    assert_eq!(host_component(b""), "unnamed");
}

fn hash_of(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
