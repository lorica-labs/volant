// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading the frames a link carries, and what the stream holds once they stop.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use volant_protocol::frame::payload_len;

/// Reads whole frames off the agent's stdout in a loop and forwards each outcome: a payload,
/// a read error, or nothing at all once the stream ends cleanly (dropping the sender, which
/// is what turns the matching `recv` into `Ok(None)`).
///
/// A header that announces more than a frame may hold is no frame: the agent never writes one,
/// so it is text the process behind the link printed after the agent, as `su` does when the
/// session ends. It is kept, with what follows it up to the end of the stream, for
/// [`super::AgentLink::ended`], and the stream ends there.
pub(super) async fn read_frames(
    mut stdout: BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    tx: mpsc::Sender<std::io::Result<Vec<u8>>>,
    trailing: Arc<Mutex<Vec<u8>>>,
) {
    loop {
        match read_frame(&mut stdout).await {
            Ok(Frame::Payload(payload)) => {
                if tx.send(Ok(payload)).await.is_err() {
                    return;
                }
            }
            Ok(Frame::Text(head)) => {
                let mut text = head.to_vec();
                let mut rest = (&mut stdout).take(TRAILING_LIMIT);
                let read = rest.read_to_end(&mut text);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), read).await;
                *trailing.lock().expect("agent trailing") = text;
                return;
            }
            Ok(Frame::End) => return,
            Err(err) => {
                let _ = tx.send(Err(err)).await;
                return;
            }
        }
    }
}

/// A shell's status for a process a signal stopped: 128 and the signal.
#[cfg(unix)]
pub(super) fn signal_code(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| 128 + signal)
}

#[cfg(not(unix))]
pub(super) fn signal_code(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

/// The most text after the last frame kept for a result.
const TRAILING_LIMIT: u64 = 64 * 1024;

/// What the stream holds next.
enum Frame {
    Payload(Vec<u8>),
    /// Four bytes that announce more than a frame may hold.
    Text([u8; 4]),
    /// The stream ended cleanly before a new frame.
    End,
}

/// Async twin of `volant_protocol::frame::read_frame`: reads the first length byte alone so
/// a clean end of stream is distinguished from the stream closing partway through the length
/// prefix or the payload, both of which are errors.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Frame> {
    let mut head = [0u8; 4];
    if r.read(&mut head[..1]).await? == 0 {
        return Ok(Frame::End);
    }
    r.read_exact(&mut head[1..]).await?;
    let Ok(len) = payload_len(head) else {
        return Ok(Frame::Text(head));
    };
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(Frame::Payload(payload))
}
