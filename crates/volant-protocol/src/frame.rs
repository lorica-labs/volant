// SPDX-License-Identifier: GPL-3.0-or-later
//! Length-prefixed frames: a big-endian `u32` length followed by that many bytes.

use std::io::{self, Read, Write};

/// Largest frame accepted. Protects against a corrupt length allocating unbounded memory.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// The four-byte header for a payload of `len` bytes.
pub fn header(len: usize) -> io::Result<[u8; 4]> {
    u32::try_from(len)
        .map(u32::to_be_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))
}

/// The payload length a header announces, refused above `MAX_FRAME_LEN`.
pub fn payload_len(header: [u8; 4]) -> io::Result<usize> {
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the {MAX_FRAME_LEN} byte limit"),
        ));
    }
    Ok(len)
}

/// Writes one frame and flushes the writer.
pub fn write_frame<W: Write>(mut w: W, payload: &[u8]) -> io::Result<()> {
    w.write_all(&header(payload.len())?)?;
    w.write_all(payload)?;
    w.flush()
}

/// Reads one frame. Returns `Ok(None)` when the stream ends cleanly before a new frame,
/// and an `UnexpectedEof` error when it ends in the middle of one.
pub fn read_frame<R: Read>(mut r: R) -> io::Result<Option<Vec<u8>>> {
    let mut first = [0u8; 1];
    if r.read(&mut first)? == 0 {
        return Ok(None);
    }
    let mut rest = [0u8; 3];
    r.read_exact(&mut rest)?;
    let len = payload_len([first[0], rest[0], rest[1], rest[2]])?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some(payload))
}

#[cfg(test)]
mod tests;
