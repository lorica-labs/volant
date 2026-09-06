// SPDX-License-Identifier: GPL-3.0-or-later
use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use volant_protocol::frame::{read_frame, write_frame};
use volant_protocol::{FromAgent, ToAgent};

pub struct Agent {
    pub child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

pub fn spawn_agent() -> Agent {
    let mut child = Command::new(env!("CARGO_BIN_EXE_volant-agent"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("agent starts");
    let stdin = child.stdin.take();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    Agent {
        child,
        stdin,
        stdout,
    }
}

impl Agent {
    pub fn send(&mut self, msg: &ToAgent) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        write_frame(&mut *stdin, &serde_json::to_vec(msg).unwrap()).unwrap();
        stdin.flush().unwrap();
    }

    /// Writes raw bytes with no framing, for tests that need to send a partial frame.
    /// Only `handshake.rs` calls this; each integration test binary compiles its own
    /// copy of this module, so the others see it as unused.
    #[allow(dead_code)]
    pub fn write_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    pub fn recv(&mut self) -> Option<FromAgent> {
        read_frame(&mut self.stdout)
            .unwrap()
            .map(|b| serde_json::from_slice(&b).unwrap())
    }

    /// Closes stdin so the agent sees end of stream.
    pub fn close(&mut self) {
        self.stdin.take();
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
