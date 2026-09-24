// SPDX-License-Identifier: GPL-3.0-or-later
mod common;

use common::spawn_agent;
use volant_protocol::{FromAgent, PROTOCOL_VERSION, ToAgent};

#[test]
fn hello_gets_ready_and_eof_ends_the_agent() {
    let mut agent = spawn_agent();
    agent.send(&ToAgent::Hello {
        protocol: PROTOCOL_VERSION,
    });
    match agent.recv() {
        Some(FromAgent::Ready {
            protocol,
            version,
            arch,
            interpreters,
            ..
        }) => {
            assert_eq!(protocol, PROTOCOL_VERSION);
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
            assert!(!arch.is_empty());
            // Only the shape, not the count: what this host has is the discovery tests'
            // business. A relative path here would be one the controller cannot execute.
            for path in &interpreters {
                assert!(path.starts_with('/'), "not absolute: {path}");
            }
        }
        other => panic!("expected Ready, got {other:?}"),
    }
    agent.close();
    assert_eq!(agent.recv(), None, "no more frames after stdin closes");
    assert!(agent.child.wait().unwrap().success());
}

#[test]
fn a_truncated_frame_makes_the_agent_exit_non_zero() {
    let mut agent = spawn_agent();
    // Declares a 5 byte payload, then stdin closes before any payload arrives.
    agent.write_raw(&5u32.to_be_bytes());
    agent.close();
    let status = agent.child.wait().unwrap();
    assert!(!status.success(), "expected a non-zero exit, got {status}");
}

#[test]
fn a_protocol_mismatch_is_logged_before_ready() {
    let mut agent = spawn_agent();
    agent.send(&ToAgent::Hello {
        protocol: PROTOCOL_VERSION + 1,
    });
    assert!(matches!(agent.recv(), Some(FromAgent::Log { .. })));
    assert!(matches!(agent.recv(), Some(FromAgent::Ready { .. })));
    agent.close();
    agent.child.wait().unwrap();
}

/// The agent reports the interpreters it found on the `PATH` it was given, so `serve` has to
/// call discovery rather than hand over a list it built from nothing.
///
/// What would make this red: `interpreters` wired to `Vec::new()`. Every other test in the suite
/// stays green under that change - the unit tests drive discovery directly, and an empty list is
/// byte for byte what a field that was never filled looks like on the wire - while every host on
/// earth reports no Python.
///
/// `first()` rather than the whole list: `/usr/bin/python3` is a candidate whatever `PATH` says,
/// so only the head of the list is this test's to decide. `python3.12` comes before it in the
/// reference's order, and a `PATH` of one directory cannot produce a `python3.13`.
#[cfg(unix)]
#[test]
fn ready_names_the_interpreter_on_the_path_the_agent_was_given() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("volant-handshake-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let python = dir.join("python3.12");
    std::fs::write(&python, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut agent = common::spawn_agent_with_path(dir.as_os_str());
    agent.send(&ToAgent::Hello {
        protocol: PROTOCOL_VERSION,
    });
    match agent.recv() {
        Some(FromAgent::Ready { interpreters, .. }) => assert_eq!(
            interpreters.first().map(String::as_str),
            Some(python.to_str().unwrap()),
            "the agent reported {interpreters:?}"
        ),
        other => panic!("expected Ready, got {other:?}"),
    }
    agent.close();
    agent.child.wait().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
