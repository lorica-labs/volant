// SPDX-License-Identifier: GPL-3.0-or-later
//! Volant agent: runs task batches on a managed host, talking frames on stdin and stdout.

mod blobs;
mod clock;
mod interpreter;
mod modules;
mod python;
mod runner;

use std::io::{self, BufReader, BufWriter};
use std::sync::mpsc;
use std::thread;

use volant_protocol::frame::{read_frame, write_frame};
use volant_protocol::{FromAgent, LogLevel, PROTOCOL_VERSION, ToAgent};

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        if std::env::args().nth(2).as_deref() != Some("--build-id") {
            println!("volant-agent {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        match own_hash() {
            Ok(hash) => println!("volant-agent {} {hash}", env!("CARGO_PKG_VERSION")),
            Err(err) => {
                eprintln!("volant-agent: reading its own file: {err}");
                std::process::exit(1);
            }
        }
        return;
    }
    let remote_tmp = blobs::remote_tmp();
    blobs::sweep(&remote_tmp);
    let served = serve(&remote_tmp);
    // Whichever way the conversation ended. A panic aborts the release build before this line,
    // and a killed agent never reaches it: both leave their directory to the next agent's sweep.
    blobs::end_connection(&remote_tmp);
    if let Err(err) = served {
        eprintln!("volant-agent: {err}");
        std::process::exit(1);
    }
}

/// The blake3 hash of the file this process runs from, which the controller compares with the
/// agent it would upload before reusing a cached one. `/proc/self/exe` is the running file even
/// when another upload has renamed a new one over its path since; elsewhere the path is all
/// there is.
fn own_hash() -> io::Result<String> {
    let bytes = std::fs::read("/proc/self/exe")
        .or_else(|_| std::env::current_exe().and_then(std::fs::read))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn serve(remote_tmp: &str) -> io::Result<()> {
    // A reader thread turns stdin into messages so the executor can notice `Cancel`
    // while a task is running.
    let (tx, rx) = mpsc::channel::<io::Result<ToAgent>>();
    thread::spawn(move || {
        let mut stdin = BufReader::new(io::stdin().lock());
        loop {
            match read_frame(&mut stdin) {
                Ok(Some(bytes)) => match serde_json::from_slice::<ToAgent>(&bytes) {
                    Ok(msg) => {
                        if tx.send(Ok(msg)).is_err() {
                            break;
                        }
                    }
                    Err(err) => eprintln!("volant-agent: discarding malformed frame: {err}"),
                },
                Ok(None) => break,
                Err(err) => {
                    let _ = tx.send(Err(err));
                    break;
                }
            }
        }
    });

    let mut out = BufWriter::new(io::stdout().lock());
    let mut send =
        |msg: &FromAgent| -> io::Result<()> { write_frame(&mut out, &serde_json::to_vec(msg)?) };

    while let Ok(msg) = rx.recv() {
        let msg = match msg {
            Ok(msg) => msg,
            Err(err) => {
                send(&FromAgent::Log {
                    level: LogLevel::Error,
                    message: format!("reading frame from controller: {err}"),
                })?;
                return Err(err);
            }
        };
        match msg {
            ToAgent::Hello { protocol } => {
                if protocol != PROTOCOL_VERSION {
                    send(&FromAgent::Log {
                        level: LogLevel::Warn,
                        message: format!(
                            "controller speaks protocol {protocol}, agent speaks {PROTOCOL_VERSION}"
                        ),
                    })?;
                }
                send(&FromAgent::Ready {
                    protocol: PROTOCOL_VERSION,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    arch: std::env::consts::ARCH.to_string(),
                    interpreters: interpreter::discover(),
                })?;
            }
            ToAgent::RunBatch { id, tasks } => runner::run_batch(id, &tasks, &rx, &mut send)?,
            ToAgent::Cancel { .. } => {}
            // Answered from the bytes, not from a file of that name: see the head of `blobs.rs`.
            // The same answer is given mid-batch by `runner::is_cancelled`, which is why it
            // lives in `blobs` rather than here.
            ToAgent::HasBlob { .. } | ToAgent::PutBlob { .. } => {
                blobs::answer(remote_tmp, &msg, &mut send)?;
            }
        }
    }
    Ok(())
}
