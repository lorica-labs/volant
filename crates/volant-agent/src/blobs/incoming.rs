// SPDX-License-Identifier: GPL-3.0-or-later

//! Reading a message from the controller, and the blob frame that follows a `put_blob`.

use std::borrow::Cow;
use std::io::{self, Read};

use volant_protocol::encoding::{BLOB_LIMIT, inflate};
use volant_protocol::frame::payload_len;
use volant_protocol::{BlobEncoding, ToAgent};

/// A message from the controller, with the frame that follows it when it is a `put_blob`: the
/// frame's bytes, or why they were skipped. Empty for every other message.
pub type Incoming = (ToAgent, Result<Vec<u8>, String>);

/// Reads one message and, after a `put_blob`, the blob frame that always follows it, so the
/// stream never falls out of step: a frame longer than the announced `len` is read through and
/// dropped without being held, and answered as a refusal. `Ok(None)` at a clean end of stream.
///
/// A message that does not read is `Some(Err(why))`: the caller logs and drops it, as it always
/// has. A `put_blob` that does not read (an encoding this agent does not know) still has its
/// blob frame after it, which is read through without being held, never taken for the next
/// message; when it names its hash it is answered as a refused blob.
pub fn read_incoming<R: Read>(mut input: R) -> io::Result<Option<Result<Incoming, String>>> {
    let Some(bytes) = volant_protocol::frame::read_frame(&mut input)? else {
        return Ok(None);
    };
    let msg = match serde_json::from_slice::<ToAgent>(&bytes) {
        Ok(msg) => msg,
        Err(err) => return unreadable(&mut input, &bytes, &err.to_string()),
    };
    let blob = match &msg {
        ToAgent::PutBlob { len, .. } => read_blob_frame(&mut input, *len)?,
        _ => Ok(Vec::new()),
    };
    Ok(Some(Ok((msg, blob))))
}

/// A message that did not read, after its frame: the blob frame behind a `put_blob` is read
/// through, and a `put_blob` naming its hash comes back as one whose blob is refused, so the
/// controller waiting for its `BlobState` gets one.
fn unreadable<R: Read>(
    mut input: R,
    bytes: &[u8],
    why: &str,
) -> io::Result<Option<Result<Incoming, String>>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).unwrap_or_default();
    if value.get("type").and_then(|t| t.as_str()) != Some("put_blob") {
        return Ok(Some(Err(why.to_string())));
    }
    // Announced as 0 bytes, any frame is longer and read through without being held.
    let _skipped = read_blob_frame(&mut input, 0)?;
    let Some(hash) = value.get("hash").and_then(|h| h.as_str()) else {
        return Ok(Some(Err(why.to_string())));
    };
    let refused = ToAgent::PutBlob {
        hash: hash.to_string(),
        len: 0,
        encoding: BlobEncoding::Raw,
        staged: value.get("staged").and_then(serde_json::Value::as_bool) == Some(true),
    };
    Ok(Some(Ok((
        refused,
        Err(format!("the put_blob does not read: {why}")),
    ))))
}

/// The frame after a `put_blob` announcing `len` decoded bytes. Neither encoding makes a blob
/// longer than `len` on the wire (the controller deflates only when that is shorter), so a
/// longer frame, or any frame after a `len` past [`BLOB_LIMIT`], is read through without being
/// held and refused. The end of the stream in its place is an error: the controller went away
/// mid-blob.
fn read_blob_frame<R: Read>(mut input: R, len: u64) -> io::Result<Result<Vec<u8>, String>> {
    let mut header = [0u8; 4];
    input.read_exact(&mut header)?;
    let size = payload_len(header)?;
    if size as u64 > len || len > BLOB_LIMIT as u64 {
        io::copy(&mut input.take(size as u64), &mut io::sink())?;
        return Ok(Err(if len > BLOB_LIMIT as u64 {
            format!("{len} bytes is past the {BLOB_LIMIT} byte limit")
        } else {
            format!("the blob's frame is {size} bytes, longer than the {len} bytes it decodes to")
        }));
    }
    let mut bytes = vec![0u8; size];
    input.read_exact(&mut bytes)?;
    Ok(Ok(bytes))
}

/// The blob a `put_blob`'s frame carries, decoded: refused when `len` is past [`BLOB_LIMIT`],
/// when a raw frame is not `len` bytes, when a deflated one does not inflate to exactly `len`
/// bytes (truncated, damaged, or a bomb stopped at `len`).
pub(super) fn decoded(len: u64, encoding: BlobEncoding, frame: &[u8]) -> io::Result<Cow<'_, [u8]>> {
    let invalid = |why: String| io::Error::new(io::ErrorKind::InvalidData, why);
    let len = usize::try_from(len)
        .ok()
        .filter(|len| *len <= BLOB_LIMIT)
        .ok_or_else(|| invalid(format!("{len} bytes is past the {BLOB_LIMIT} byte limit")))?;
    let bytes = match encoding {
        BlobEncoding::Raw => Cow::Borrowed(frame),
        BlobEncoding::Deflate => Cow::Owned(inflate(frame, len).map_err(invalid)?),
    };
    if bytes.len() != len {
        return Err(invalid(format!(
            "the blob decoded to {} bytes where {len} were announced",
            bytes.len()
        )));
    }
    Ok(bytes)
}
