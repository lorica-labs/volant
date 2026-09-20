// SPDX-License-Identifier: GPL-3.0-or-later
//! Volant agent: runs task batches on a managed host, talking frames on stdin and stdout.

mod blobs;
mod clock;
mod modules;
mod runner;

use std::io::{self, BufReader, BufWriter};
use std::sync::mpsc;
use std::thread;

use volant_protocol::frame::{read_frame, write_frame};
use volant_protocol::{FromAgent, LogLevel, PROTOCOL_VERSION, ToAgent};

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("volant-agent {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Err(err) = serve() {
        eprintln!("volant-agent: {err}");
        std::process::exit(1);
    }
}

fn serve() -> io::Result<()> {
    let remote_tmp = blobs::remote_tmp();
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
                })?;
            }
            ToAgent::RunBatch { id, tasks } => runner::run_batch(id, &tasks, &rx, &mut send)?,
            ToAgent::Cancel { .. } => {}
            // Answered from the bytes, not from a file of that name: see the head of `blobs.rs`.
            // A cache this agent cannot trust is a log and a `false`, never a silent one.
            ToAgent::HasBlob { hash } => {
                let present = match blobs::holds(&remote_tmp, &hash) {
                    Ok(present) => present,
                    Err(err) => {
                        send(&FromAgent::Log {
                            level: LogLevel::Error,
                            message: format!("looking for payload {hash}: {err}"),
                        })?;
                        false
                    }
                };
                send(&FromAgent::BlobState { hash, present })?;
            }
            // A refused payload is answered, never left silent: the controller waits for this
            // state before it sends the batch that needs the payload, and the log is the only
            // place the reason survives.
            ToAgent::PutBlob { hash, zip_b64 } => {
                match blobs::store(&remote_tmp, &hash, &zip_b64) {
                    Ok(_) => send(&FromAgent::BlobState {
                        hash,
                        present: true,
                    })?,
                    Err(err) => {
                        send(&FromAgent::Log {
                            level: LogLevel::Error,
                            message: format!("storing payload {hash}: {err}"),
                        })?;
                        send(&FromAgent::BlobState {
                            hash,
                            present: false,
                        })?;
                    }
                }
            }
        }
    }
    Ok(())
}
