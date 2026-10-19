//! Wire protocol: the control messages exchanged over the encrypted
//! [`SecureStream`], plus length-delimited (de)serialization helpers.
//!
//! Framing: every control message is a UTF-8 JSON document prefixed with its
//! length as a big-endian `u32`. Bulk chunk payloads are *not* JSON — a
//! [`ChunkHeader`] control message is sent first, immediately followed by
//! exactly `len` raw (still end-to-end encrypted) bytes on the same stream.
//!
//! JSON is used here for clarity and debuggability at Phase 2; the format is
//! versioned via [`PROTOCOL_VERSION`] so a compact binary encoding can be
//! introduced later without breaking older peers.

use crate::error::{Error, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{Read, Write};

/// Bumped on any incompatible change to messages or transfer semantics.
pub const PROTOCOL_VERSION: u32 = 1;

/// Control messages exchanged between sender (initiator) and receiver
/// (responder). The variant name is serialized as a `type` tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Control {
    /// First message after the handshake: identifies the sender and protocol.
    Hello {
        protocol_version: u32,
        device_name: String,
        /// Sender's own static-key fingerprint (informational; the receiver
        /// independently derives it from the authenticated handshake).
        fingerprint: String,
    },
    /// Receiver's response to [`Control::Hello`].
    HelloAck {
        accepted: bool,
        reason: String,
        device_name: String,
    },
    /// Sender proposes a set of files to transfer under one transfer id.
    Offer {
        transfer_id: String,
        files: Vec<FileOffer>,
    },
    /// Receiver's per-file plan: which chunks it still needs (enables resume).
    OfferResponse { files: Vec<FilePlan> },
    /// Header preceding a raw chunk payload of exactly `len` bytes.
    ChunkHeader {
        file_index: u32,
        chunk_index: u64,
        offset: u64,
        len: u32,
        /// BLAKE3 hash (hex) of the chunk payload.
        hash: String,
    },
    /// Sender has sent all needed chunks for a file; carries the whole-file
    /// hash so the receiver can verify the reconstruction before committing.
    FileDone { file_index: u32, file_hash: String },
    /// Receiver's verdict for a finalized file.
    FileResult {
        file_index: u32,
        ok: bool,
        error: String,
    },
    /// Sender signals the whole transfer is complete.
    Complete,
    /// Out-of-band error from either side; the connection is torn down after.
    Error { message: String },

    // --- Synchronization (Phase 3) ---------------------------------------
    /// Sender proposes a one-way directory sync. Unlike [`Control::Offer`],
    /// the receiver compares each file against the *actual* destination tree
    /// (size + mtime, or content hash when `checksum`) and skips identical
    /// files, and may report/perform deletions of extraneous destination files.
    SyncOffer {
        sync_id: String,
        files: Vec<FileOffer>,
        /// Delete destination files not present in the source set (gated).
        delete: bool,
        /// Compute the plan but make no changes on either side.
        dry_run: bool,
        /// Compare file contents by hash rather than size + mtime.
        checksum: bool,
        /// Use delta transfer for changed files that already exist.
        #[serde(default)]
        delta: bool,
        /// Exclude globs (already applied by the sender); the receiver also
        /// uses them to protect matching destination files from deletion.
        excludes: Vec<String>,
    },
    /// Receiver's answer to a [`Control::SyncOffer`]: per-file plan plus the
    /// set of destination paths that are extraneous (would be / were deleted)
    /// and any detected conflicts.
    SyncResponse {
        files: Vec<FilePlan>,
        deletions: Vec<String>,
        conflicts: Vec<String>,
    },
    /// Receiver's final message after a sync: how many files it deleted.
    SyncComplete { deleted: u64 },

    // --- Delta transfer (Phase 3) ----------------------------------------
    /// Sender asks the receiver for block signatures of its existing copy.
    DeltaRequest { file_index: u32 },
    /// Receiver's block signatures for its basis file: `(weak, strong-hex)`.
    DeltaSignatures {
        file_index: u32,
        block_size: u32,
        sigs: Vec<(u32, String)>,
    },
    /// Copy `count` consecutive basis blocks starting at `start_block`.
    DeltaCopy {
        file_index: u32,
        start_block: u64,
        count: u64,
    },
    /// Literal run of exactly `len` bytes follows on the stream.
    DeltaLiteral { file_index: u32, len: u32 },
    /// Sender finished the delta; `file_hash` is the whole new-file BLAKE3.
    DeltaDone { file_index: u32, file_hash: String },
}

/// A file proposed in an [`Control::Offer`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOffer {
    /// Relative path under the transfer root (validated by the receiver).
    pub rel_path: String,
    /// Total file size in bytes (64-bit).
    pub size: u64,
    /// Chunk size chosen by the sender for this file.
    pub chunk_size: u32,
    /// Optional Unix mode bits to preserve (receiver may ignore).
    pub mode: Option<u32>,
    /// Source modification time (Unix seconds), if available. Combined with
    /// `size` it forms a cheap content signature: if either differs from a
    /// prior attempt, the receiver discards partial data and restarts the file.
    pub mtime: Option<u64>,
    /// Whole-file BLAKE3 hash (hex), populated only for `sync --checksum` so
    /// the receiver can compare contents rather than size + mtime. `None` on
    /// the normal push path (the sender streams and verifies anyway).
    #[serde(default)]
    pub content_hash: Option<String>,
}

/// The receiver's plan for a single offered file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePlan {
    pub file_index: u32,
    /// Which chunks the receiver still needs.
    pub needed: ChunkSelection,
    /// True if some verified chunks were reused from a prior attempt.
    pub resume: bool,
    /// True if the sender should send this file via delta (the receiver has an
    /// older copy to reuse as the basis). `needed` is ignored when set.
    #[serde(default)]
    pub delta: bool,
}

/// Selection of chunks the receiver wants, kept compact for the common cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum ChunkSelection {
    /// All chunks (fresh transfer).
    All,
    /// A specific set of chunk indices (partial resume).
    Indices(Vec<u64>),
    /// Nothing needed — file already present and verified.
    None,
}

/// Write a control message with a `u32` big-endian length prefix.
pub fn write_control<W: Write>(w: &mut W, msg: &Control) -> Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| Error::protocol("control message exceeds u32 length"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Read a length-prefixed control message, rejecting anything larger than
/// `max_bytes` (defends against a peer claiming a huge message).
pub fn read_control<R: Read>(r: &mut R, max_bytes: usize) -> Result<Control> {
    read_framed(r, max_bytes)
}

/// Generic length-delimited JSON reader with a hard size cap.
fn read_framed<R: Read, T: DeserializeOwned>(r: &mut R, max_bytes: usize) -> Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max_bytes {
        return Err(Error::LimitExceeded(format!(
            "control message of {len} bytes exceeds cap of {max_bytes}"
        )));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}
