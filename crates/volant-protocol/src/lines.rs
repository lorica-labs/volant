// SPDX-License-Identifier: GPL-3.0-or-later
//! The link's bytes as base64 lines, for a channel that only text crosses whole: a Session Manager
//! shell hands the instance's output to the controller as UTF-8, dropping or holding back bytes
//! that are not, and the instance's terminal stalls a line longer than 4095 bytes. Each line
//! carries at most [`CHUNK`] bytes and decodes on its own, so a writer may cut anywhere. The line
//! [`END`] is the end of the input, which a terminal in raw mode has no other way to say. It is
//! not an empty line: a Session Manager session turns a newline sent on its own into `\r`.

use std::io::{self, BufRead, Read, Write};

use crate::encoding::{b64_decode, b64_encode};

/// The most one line carries: 4000 characters of base64 and a newline.
pub const CHUNK: usize = 3000;

/// The line that ends the input: a dot, which base64 never holds.
pub const END: &[u8] = b".\n";

/// The line that carries `bytes`, at most [`CHUNK`] of them.
pub fn line(bytes: &[u8]) -> Vec<u8> {
    let mut line = b64_encode(bytes).into_bytes();
    line.push(b'\n');
    line
}

/// The bytes one line carries, its newline and a carriage return before it left out.
///
/// # Errors
/// A line that is not base64.
pub fn decode(line: &[u8]) -> io::Result<Vec<u8>> {
    let text = line.strip_suffix(b"\n").unwrap_or(line);
    let text = text.strip_suffix(b"\r").unwrap_or(text);
    let text =
        std::str::from_utf8(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    b64_decode(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The bytes `inner`'s lines carry.
pub struct Reader<R> {
    inner: R,
    line: Vec<u8>,
    decoded: Vec<u8>,
    at: usize,
}

impl<R: BufRead> Reader<R> {
    pub fn new(inner: R) -> Self {
        Reader {
            inner,
            line: Vec::new(),
            decoded: Vec::new(),
            at: 0,
        }
    }
}

impl<R: BufRead> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.decoded.len() {
            self.line.clear();
            if self.inner.read_until(b'\n', &mut self.line)? == 0 {
                return Ok(0);
            }
            if self.line.trim_ascii_end() == END.trim_ascii_end() {
                return Ok(0);
            }
            self.decoded = decode(&self.line)?;
            self.at = 0;
        }
        let n = buf.len().min(self.decoded.len() - self.at);
        buf[..n].copy_from_slice(&self.decoded[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Writes what it is given to `inner` as lines, one line for each [`CHUNK`] of a write.
pub struct Writer<W> {
    inner: W,
}

impl<W: Write> Writer<W> {
    pub fn new(inner: W) -> Self {
        Writer { inner }
    }
}

impl<W: Write> Write for Writer<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(CHUNK);
        if n > 0 {
            self.inner.write_all(&line(&buf[..n]))?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests;
