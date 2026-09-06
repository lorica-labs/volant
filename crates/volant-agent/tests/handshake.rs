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
        }) => {
            assert_eq!(protocol, PROTOCOL_VERSION);
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
            assert!(!arch.is_empty());
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
