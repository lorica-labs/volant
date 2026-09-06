// SPDX-License-Identifier: GPL-3.0-or-later
//! Volant agent: runs task batches on a managed host, talking frames on stdin and stdout.

// Not yet called from `serve`: the controller does not drive batches yet.
#[allow(dead_code)]
mod clock;
#[allow(dead_code)]
mod modules;

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
            ToAgent::RunBatch { id, .. } => send(&FromAgent::Log {
                level: LogLevel::Error,
                message: format!("batch {id} refused: task execution is not implemented"),
            })?,
            ToAgent::Cancel { .. } => {}
        }
    }
    Ok(())
}
