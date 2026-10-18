// SPDX-License-Identifier: GPL-3.0-or-later
//! The blobs sent to an agent: their frames, the process-wide cache of frames, and what a link remembers.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use volant_protocol::BlobEncoding;

/// A blob as it goes on the wire in the frame after its `put_blob`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobFrame {
    /// The decoded length, which the `put_blob` announces.
    pub len: u64,
    pub encoding: BlobEncoding,
    pub bytes: Vec<u8>,
}

impl BlobFrame {
    /// `raw` as it is.
    pub fn raw(raw: Vec<u8>) -> BlobFrame {
        BlobFrame {
            len: raw.len() as u64,
            encoding: BlobEncoding::Raw,
            bytes: raw,
        }
    }

    /// `deflated` when it is shorter than `raw`, whose deflated form it is, and `raw` otherwise:
    /// the agent refuses a frame longer than the blob, so a deflate that grew is never sent.
    pub fn smaller(raw: Vec<u8>, deflated: Vec<u8>) -> BlobFrame {
        if deflated.len() < raw.len() {
            BlobFrame {
                len: raw.len() as u64,
                encoding: BlobEncoding::Deflate,
                bytes: deflated,
            }
        } else {
            BlobFrame::raw(raw)
        }
    }

    /// `raw`, deflated first when it is past 4 KiB and that makes it shorter. Under that, the
    /// deflate costs more than the bytes it saves. Past 64 KiB, the first 64 KiB are deflated
    /// alone first, and bytes whose sample does not shrink by a tenth (an archive, an image, a
    /// binary) go raw without paying a deflate of the whole file.
    pub fn packed(raw: Vec<u8>) -> BlobFrame {
        const SAMPLE: usize = 64 * 1024;
        if raw.len() <= 4096 {
            return BlobFrame::raw(raw);
        }
        if raw.len() > SAMPLE
            && volant_protocol::encoding::deflate(&raw[..SAMPLE]).len() > SAMPLE / 10 * 9
        {
            return BlobFrame::raw(raw);
        }
        #[cfg(test)]
        DEFLATED.fetch_add(raw.len() as u64, std::sync::atomic::Ordering::Relaxed);
        let deflated = volant_protocol::encoding::deflate(&raw);
        BlobFrame::smaller(raw, deflated)
    }
}

/// How many bytes [`BlobFrame::packed`] deflated whole, so a test can tell a deflate that ran
/// from one that was skipped or shared.
#[cfg(test)]
pub(super) static DEFLATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One blob's frame, made by the first caller that needs it while the others wait for it.
type Slot = Arc<OnceLock<Result<Arc<BlobFrame>, String>>>;

/// The frame each blob this process sends goes up in, by the blake3 of its content: the union's,
/// kept by the union cache when it stores or loads the entry, and each staged file's, made on its
/// first send. So a blob sent to every host of a run is decoded and deflated once, not once per
/// host.
///
/// Keyed by the content hash alone, so an entry here can never be stale: other bytes are another
/// key.
static FRAMES: Mutex<BTreeMap<String, Slot>> = Mutex::new(BTreeMap::new());

/// Keeps `frame` as the one the blob named `hash` goes up in.
pub fn keep_frame(hash: &str, frame: BlobFrame) {
    FRAMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            hash.to_string(),
            Arc::new(OnceLock::from(Ok(Arc::new(frame)))),
        );
}

/// Forgets every frame this process kept, as a new run starts with none.
#[cfg(test)]
pub fn forget_frames() {
    FRAMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

/// The frame the blob named `hash`, whose bytes are `b64`, goes up in: the one kept for it, or
/// made now from `b64` by [`BlobFrame::packed`], once for the whole process however many hosts
/// ask at the same moment, and on a blocking thread rather than on an async worker.
pub async fn frame(hash: &str, b64: &str) -> Result<Arc<BlobFrame>, String> {
    let slot = Arc::clone(
        FRAMES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(hash.to_string())
            .or_default(),
    );
    if let Some(made) = slot.get() {
        return made.clone();
    }
    let (named, b64) = (hash.to_string(), b64.to_string());
    tokio::task::spawn_blocking(move || {
        slot.get_or_init(|| {
            volant_protocol::encoding::b64_decode(&b64)
                .map(|raw| Arc::new(BlobFrame::packed(raw)))
                .map_err(|err| format!("decoding the blob {named}: {err}"))
        })
        .clone()
    })
    .await
    .map_err(|err| format!("framing the blob {hash}: {err}"))?
}

/// What one link knows about the module payloads the agent behind it holds: `Ok` for a payload
/// the agent confirmed, `Err` for one it refused, with the sentence the refusal failed under.
///
/// It belongs to the link and to nothing longer-lived, which is the whole answer to a
/// connection lost and reopened: the new link is a new `AgentLink` carrying an empty memory, so
/// its first batch asks again rather than assuming what the link before it was told. A memory
/// kept per host, or per run, would send a batch that needs a payload to an agent that never
/// received one.
#[derive(Debug, Default)]
pub struct BlobMemory(HashMap<String, Result<(), String>>);

impl BlobMemory {
    /// What this link was told about `hash`, if it has been told anything.
    pub fn seen(&self, hash: &str) -> Option<Result<(), String>> {
        self.0.get(hash).cloned()
    }

    pub fn remember(&mut self, hash: &str, state: Result<(), String>) {
        self.0.insert(hash.to_string(), state);
    }

    /// Whether the agent behind this link confirmed holding this payload. A refusal is not a
    /// hold, which is why this reads the value rather than the key.
    pub fn holds(&self, hash: &str) -> bool {
        matches!(self.0.get(hash), Some(Ok(())))
    }
}
