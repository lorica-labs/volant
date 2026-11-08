// SPDX-License-Identifier: GPL-3.0-or-later
//! A password prompt on the controlling terminal while stdin is something else, the way `sshpass`
//! runs `ansible-playbook -k`: `getpass` reads `/dev/tty`, and so does Volant.
#![cfg(unix)]

use std::io::Read as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn terminal() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: both descriptors are written by `openpty` and owned by nothing else.
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "a pseudo-terminal opens");
    // SAFETY: `openpty` just returned them, and each is wrapped once.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// Measured on ansible-core 2.19.12: `sshpass -p secret ansible-playbook -k` with stdin at
/// `/dev/null` runs, and the answer typed on the terminal is not echoed back.
#[test]
fn the_ssh_password_is_read_from_the_terminal_with_stdin_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("p.yml");
    std::fs::write(
        &book,
        "- hosts: localhost\n  gather_facts: false\n  tasks:\n    - debug: {msg: ran}\n",
    )
    .unwrap();
    let (master, slave) = terminal();
    let tty = slave.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_volant"));
    command
        .args(["playbook", "-k", "-c", "local"])
        .arg(&book)
        .env("NO_COLOR", "1")
        .env("VOLANT_RUNTIME", "system")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe calls between fork and exec: a session of its own, with the
    // terminal as its controlling one and none of its standard streams.
    unsafe {
        command.pre_exec(move || {
            libc::setsid();
            // The request's type differs between Linux and macOS.
            libc::ioctl(tty, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = command.spawn().expect("volant starts");
    drop(slave);
    let mut screen = std::fs::File::from(master.try_clone().unwrap());
    let shown = std::thread::spawn(move || {
        let mut text = Vec::new();
        let mut buf = [0u8; 256];
        // The read fails with EIO once the last holder of the terminal has gone.
        while let Ok(n @ 1..) = screen.read(&mut buf) {
            text.extend_from_slice(&buf[..n]);
            if text.ends_with(b"SSH password: ") {
                // SAFETY: the bytes are valid for their length and `master` is open.
                unsafe { libc::write(master.as_raw_fd(), b"secret\n".as_ptr().cast(), 7) };
            }
        }
        String::from_utf8_lossy(&text).into_owned()
    });
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(60) {
            child.kill().unwrap();
            panic!("the run waits for an answer it was given");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    let shown = shown.join().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "the run ends well: {stdout}");
    assert!(stdout.contains("\"msg\": \"ran\""), "{stdout}");
    assert!(shown.starts_with("SSH password: "), "{shown:?}");
    assert!(!shown.contains("secret"), "the answer is echoed: {shown:?}");
}
