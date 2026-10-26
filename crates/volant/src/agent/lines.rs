// SPDX-License-Identifier: GPL-3.0-or-later
//! The controller's half of `volant_protocol::lines`: a link whose bytes travel as base64 lines,
//! for a channel that only text crosses whole.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};
use volant_protocol::lines::{CHUNK, decode, line};

/// Writes what it is given to `inner` as lines, one line for each [`CHUNK`] of a write. A line is
/// sent in full before the next write is taken, and on a flush.
pub(crate) struct LineWriter<W> {
    inner: W,
    pending: Vec<u8>,
    at: usize,
}

impl<W> LineWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        LineWriter {
            inner,
            pending: Vec::new(),
            at: 0,
        }
    }
}

impl<W: AsyncWrite + Unpin> LineWriter<W> {
    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.at < self.pending.len() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.at..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.at += n;
        }
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for LineWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        ready!(self.drain(cx))?;
        let n = buf.len().min(CHUNK);
        if n > 0 {
            self.pending = line(&buf[..n]);
            self.at = 0;
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.drain(cx))?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.drain(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The bytes `inner`'s lines carry. A line that is not base64, such as a shell's complaint, is
/// passed on as it is, for the link to show among what the stream said after its last frame.
pub(crate) struct LineReader<R> {
    inner: R,
    line: Vec<u8>,
    decoded: Vec<u8>,
    at: usize,
}

impl<R> LineReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        LineReader {
            inner,
            line: Vec::new(),
            decoded: Vec::new(),
            at: 0,
        }
    }
}

impl<R: AsyncBufRead + Unpin> AsyncRead for LineReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        while this.at == this.decoded.len() {
            let available = ready!(Pin::new(&mut this.inner).poll_fill_buf(cx))?;
            if available.is_empty() {
                // The end of the stream: a last line with no newline is what it carried.
                if this.line.is_empty() {
                    return Poll::Ready(Ok(()));
                }
                this.decoded = std::mem::take(&mut this.line);
                this.at = 0;
                break;
            }
            if let Some(end) = available.iter().position(|b| *b == b'\n') {
                this.line.extend_from_slice(&available[..=end]);
                Pin::new(&mut this.inner).consume(end + 1);
                let whole = std::mem::take(&mut this.line);
                this.decoded = decode(&whole).unwrap_or(whole);
                this.at = 0;
            } else {
                let n = available.len();
                this.line.extend_from_slice(available);
                Pin::new(&mut this.inner).consume(n);
            }
        }
        let n = buf.remaining().min(this.decoded.len() - this.at);
        buf.put_slice(&this.decoded[this.at..this.at + n]);
        this.at += n;
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

    use super::{LineReader, LineWriter};

    #[tokio::test]
    async fn bytes_written_as_lines_read_back_whole_and_text_passes_through() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 13 % 256) as u8).collect();
        let mut wire = Vec::new();
        {
            let mut writer = LineWriter::new(&mut wire);
            writer.write_all(&data).await.unwrap();
            writer.flush().await.unwrap();
        }
        assert!(wire.split(|b| *b == b'\n').all(|line| line.len() < 4095));
        wire.extend_from_slice(b"sh: 1: verify: not found\n");
        let mut back = Vec::new();
        LineReader::new(BufReader::new(wire.as_slice()))
            .read_to_end(&mut back)
            .await
            .unwrap();
        let mut expected = data;
        expected.extend_from_slice(b"sh: 1: verify: not found\n");
        assert_eq!(back, expected);
    }
}
