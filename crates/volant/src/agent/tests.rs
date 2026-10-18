// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::testing::scratch;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const EMBEDDED: &[(&str, &[u8])] = &[("volant-agent", b"embedded")];

fn agent_in(dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, b"on disk").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Every link of a run compares against, and uploads, the bytes the first one read.
#[test]
fn an_agent_file_is_read_and_hashed_once_per_source() {
    let tmp = scratch();
    let dir = tmp.path().to_path_buf();
    let path = agent_in(&dir, "volant-agent-x");
    let source = AgentSource::ordered(Some(dir.clone()), None, None);
    let first = source.read(&path).unwrap();
    assert_eq!(first.bytes, b"on disk");
    assert_eq!(first.hash, blake3::hash(b"on disk").to_hex().to_string());
    std::fs::write(&path, b"rebuilt").unwrap();
    let again = source.clone().read(&path).unwrap();
    assert!(Arc::ptr_eq(&first, &again), "{:?}", again.bytes);
}

/// `VOLANT_AGENT_DIR` comes first, then the embedded agents, then the controller's own
/// directory, where an unembedded agent is still found.
#[test]
fn agents_are_looked_up_in_the_environment_then_the_controller_then_beside_it() {
    let root = scratch();
    let (env_dir, exe_dir, cache) = (
        root.path().join("env"),
        root.path().join("exe"),
        root.path().join("cache"),
    );
    let from_env = agent_in(&env_dir, "volant-agent");
    agent_in(&exe_dir, "volant-agent");
    let riscv = agent_in(&exe_dir, "volant-agent-riscv64gc-unknown-linux-musl");
    let source = |env: Option<&PathBuf>| {
        AgentSource::ordered(
            env.cloned(),
            Some(embedded::Embedded::new(EMBEDDED, Ok(cache.clone()))),
            Some(exe_dir.clone()),
        )
    };

    assert_eq!(source(Some(&env_dir)).local().unwrap(), from_env);
    let extracted = source(None).local().unwrap();
    assert_eq!(extracted, cache.join("volant-agent"));
    assert_eq!(std::fs::read(&extracted).unwrap(), b"embedded");
    assert_eq!(
        source(None)
            .for_target("riscv64gc-unknown-linux-musl")
            .unwrap(),
        riscv
    );
}

/// An embedded agent that cannot be put on disk does not stop a run whose agent sits beside the
/// controller, and when nothing has one the error says why the embedded one was not used.
#[test]
fn an_agent_that_cannot_be_extracted_falls_through_and_says_why() {
    let root = scratch();
    let exe_dir = root.path().join("exe");
    // A cache anyone can write, which extraction refuses.
    let cache = root.path().join("cache");
    std::fs::create_dir(&cache).unwrap();
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o777)).unwrap();
    let source = || {
        AgentSource::ordered(
            None,
            Some(embedded::Embedded::new(EMBEDDED, Ok(cache.clone()))),
            Some(exe_dir.clone()),
        )
    };

    let err = format!("{:#}", source().local().unwrap_err());
    assert!(err.contains("mode 777"), "{err}");
    let beside = agent_in(&exe_dir, "volant-agent");
    assert_eq!(source().local().unwrap(), beside);
}

/// A controller started through a link looks for its agents next to the file the link points
/// at.
#[test]
fn the_agents_are_looked_for_beside_the_linked_file() {
    let tmp = scratch();
    let root = tmp.path();
    let release = root.join("release");
    let bin = root.join("bin");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(release.join("volant"), b"").unwrap();
    std::os::unix::fs::symlink(release.join("volant"), bin.join("volant")).unwrap();

    let found = beside_executable(bin.join("volant"));

    let expected = std::fs::canonicalize(&release).unwrap();
    assert_eq!(found, Some(expected));
}

/// A path that cannot be resolved still names a directory, so a missing file behaves as
/// before rather than dropping the only place the agents could be.
#[test]
fn an_unresolvable_path_keeps_its_own_directory() {
    let found = beside_executable(PathBuf::from("/nonexistent/volant-dir/volant"));
    assert_eq!(found, Some(PathBuf::from("/nonexistent/volant-dir")));
}

/// A link over a process that reads its stdin and says nothing, which is all this needs: the
/// memory is asserted, not the protocol.
fn link() -> AgentLink {
    let child = tokio::process::Command::new("cat")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("cat is on PATH");
    AgentLink::new(child).expect("a link over it")
}

/// A frame goes deflated only past 4 KiB and only when that is shorter.
#[test]
fn a_blob_is_deflated_only_when_it_is_large_and_that_is_shorter() {
    let small = BlobFrame::packed(vec![0; 4096]);
    assert_eq!(small, BlobFrame::raw(vec![0; 4096]));
    let large = BlobFrame::packed(vec![0; 4097]);
    assert_eq!((large.len, large.encoding), (4097, BlobEncoding::Deflate));
    assert!(large.bytes.len() < 100);
    let noise: Vec<u8> = (0..256u32)
        .flat_map(|n| *blake3::hash(&n.to_le_bytes()).as_bytes())
        .collect();
    let grew = volant_protocol::encoding::deflate(&noise);
    assert!(grew.len() >= noise.len(), "{} bytes", grew.len());
    assert_eq!(BlobFrame::packed(noise.clone()), BlobFrame::raw(noise));
}

/// Bytes whose first 64 KiB do not compress go raw without a deflate of the whole of them.
#[test]
fn bytes_whose_sample_does_not_shrink_skip_the_whole_deflate() {
    let noise: Vec<u8> = (0..32 * 1024u32)
        .flat_map(|n| *blake3::hash(&n.to_le_bytes()).as_bytes())
        .collect();
    assert_eq!(BlobFrame::packed(noise.clone()), BlobFrame::raw(noise));
    assert_eq!(DEFLATED.load(std::sync::atomic::Ordering::Relaxed), 0);
    let text = b"a log line that repeats\n".repeat(40_000);
    let packed = BlobFrame::packed(text.clone());
    assert_eq!(packed.encoding, BlobEncoding::Deflate);
    assert_eq!(
        DEFLATED.load(std::sync::atomic::Ordering::Relaxed),
        text.len() as u64
    );
}

/// A blob asked for by many hosts at once is framed once, and every host gets that frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blob_sent_to_many_hosts_is_framed_once() {
    let text = b"a log line that repeats\n".repeat(40_000);
    let hash = blake3::hash(&text).to_hex().to_string();
    let b64 = volant_protocol::encoding::b64_encode(&text);
    let hosts: Vec<_> = (0..8)
        .map(|_| {
            let (hash, b64) = (hash.clone(), b64.clone());
            tokio::spawn(async move { frame(&hash, &b64).await.unwrap() })
        })
        .collect();
    let mut frames = Vec::new();
    for host in hosts {
        frames.push(host.await.unwrap());
    }
    assert!(frames.iter().all(|f| Arc::ptr_eq(f, &frames[0])));
    assert_eq!(frames[0].encoding, BlobEncoding::Deflate);
    assert_eq!(
        DEFLATED.load(std::sync::atomic::Ordering::Relaxed),
        text.len() as u64
    );
}

/// An agent that speaks the protocol before this one is refused at the handshake with the
/// sentence every mismatch gets, before any blob could reach it in a shape it would misread.
/// The stand-in for a host that never answered says so, and has no agent to answer a frame.
#[cfg(unix)]
#[tokio::test]
async fn the_stand_in_link_has_no_agent() {
    let mut link = AgentLink::dead().expect("the stand-in starts");
    assert!(link.is_stand_in());
    let answered = link.recv().await;
    assert!(matches!(answered, Ok(None) | Err(_)));
}

#[tokio::test]
async fn an_agent_that_speaks_protocol_six_is_refused_at_the_handshake() {
    let ready = br#"{"type":"ready","protocol":6,"version":"0.1.0-alpha.7","arch":"x86_64"}"#;
    let len = u32::try_from(ready.len()).unwrap().to_be_bytes();
    let mut octal = String::new();
    for byte in len.iter().chain(ready.iter()) {
        std::fmt::Write::write_fmt(&mut octal, format_args!("\\{byte:03o}")).unwrap();
    }
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("printf '{octal}'; cat > /dev/null"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("sh is on PATH");
    let err = AgentLink::new(child)
        .unwrap()
        .handshake()
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "agent 0.1.0-alpha.7 speaks protocol 6, this controller speaks 8"
    );
}

/// What a link was told about a payload dies with that link, so a connection lost and reopened
/// asks again and the blob travels again.
#[tokio::test]
async fn a_new_link_carries_no_memory_of_the_link_before_it() {
    let mut first = link();
    first.blobs().remember("ab", Ok(()));
    assert!(first.blobs().holds("ab"));

    let mut second = link();
    assert!(
        second.blobs().seen("ab").is_none(),
        "a new link inherited what the link before it was told"
    );
}
