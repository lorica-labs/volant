// SPDX-License-Identifier: GPL-3.0-or-later
//! Length-prefixed frames: a big-endian `u32` length followed by that many bytes.

use std::io::{self, Read, Write};

/// Largest frame accepted. Protects against a corrupt length allocating unbounded memory.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// Writes one frame and flushes the writer.
pub fn write_frame<W: Write>(mut w: W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_be_bytes())?;
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
    let len = u32::from_be_bytes([first[0], rest[0], rest[1], rest[2]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the {MAX_FRAME_LEN} byte limit"),
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trips_a_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        assert_eq!(buf, [0, 0, 0, 5, b'h', b'e', b'l', b'l', b'o']);
        let read = read_frame(Cursor::new(buf)).unwrap();
        assert_eq!(read.as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn clean_end_of_stream_is_none() {
        assert_eq!(read_frame(Cursor::new(Vec::<u8>::new())).unwrap(), None);
    }

    #[test]
    fn truncated_length_is_an_error() {
        let err = read_frame(Cursor::new(vec![0, 0])).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn truncated_payload_is_an_error() {
        let err = read_frame(Cursor::new(vec![0, 0, 0, 9, b'x'])).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_length_is_rejected_before_allocating() {
        let err = read_frame(Cursor::new(vec![0xff, 0xff, 0xff, 0xff])).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
