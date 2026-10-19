//! The sending (initiator) side of a transfer.
//!
//! Flow: connect → Noise handshake → Hello/HelloAck → Offer → per-file plan →
//! stream only the needed chunks (one streaming pass per file that both hashes
//! the whole file and emits the chunks the receiver asked for) → FileDone →
//! await FileResult → Complete.
//!
//! A single pass per file keeps memory bounded and avoids reading large files
//! twice: the whole-file BLAKE3 hash and each chunk hash are computed from the
//! same sequential read.

use crate::chunk;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::identity::DeviceIdentity;
use crate::manifest::{self, SenderFile, SenderRecord, TransferState};
use crate::pathsafety;
use crate::progress::{ProgressEvent, ProgressSink};
use crate::protocol::{self, ChunkSelection, Control, FileOffer, FilePlan};
use crate::transport::{self, SecureStream};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

/// One file to send: the on-disk source and its wire-relative path.
#[derive(Debug, Clone)]
pub struct SendItem {
    pub rel_path: String,
    pub source_path: PathBuf,
}

/// Options for a send operation.
pub struct SendOptions {
    /// Reuse an existing transfer id (resume) instead of generating a new one.
    pub transfer_id: Option<String>,
    /// Force a chunk size; otherwise an adaptive size is chosen per file.
    pub chunk_size: Option<u32>,
    /// This device's display name, sent in `Hello`.
    pub device_name: String,
}

/// Summary returned to callers for reporting / exit codes.
#[derive(Debug, Clone)]
pub struct SendSummary {
    pub transfer_id: String,
    pub files: u64,
    pub bytes_sent: u64,
    pub bytes_skipped: u64,
}

/// Expand filesystem sources into a flat list of [`SendItem`]s.
///
/// * A file becomes a single item named by its file name.
/// * A directory is walked; each file becomes an item whose `rel_path` is
///   relative to the directory (so the tree is reproduced at the destination).
///
/// Symlinks are not followed, to avoid exfiltrating data outside the selected
/// tree or creating cycles.
pub fn expand_sources(sources: &[PathBuf]) -> Result<Vec<SendItem>> {
    let mut items = Vec::new();
    for src in sources {
        let meta = std::fs::symlink_metadata(src)?;
        if meta.file_type().is_symlink() {
            // Skip top-level symlinks explicitly.
            continue;
        }
        if meta.is_file() {
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| Error::Config(format!("bad source path: {}", src.display())))?;
            items.push(SendItem {
                rel_path: name,
                source_path: src.clone(),
            });
        } else if meta.is_dir() {
            let base = src.clone();
            let dir_name = base
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "dir".to_string());
            for entry in walkdir::WalkDir::new(&base).follow_links(false) {
                let entry = entry.map_err(|e| Error::other(e.to_string()))?;
                if entry.file_type().is_file() {
                    let rel = entry
                        .path()
                        .strip_prefix(&base)
                        .map_err(|e| Error::other(e.to_string()))?;
                    let wire = format!("{}/{}", dir_name, pathsafety::to_wire_rel(rel));
                    items.push(SendItem {
                        rel_path: wire,
                        source_path: entry.path().to_path_buf(),
                    });
                }
            }
        }
    }
    Ok(items)
}

/// Send `items` to `target_addr`. Persists a [`SenderRecord`] so the transfer
/// can be listed, inspected, and resumed later.
pub fn send(
    config: &Config,
    identity: &DeviceIdentity,
    target_addr: &str,
    items: Vec<SendItem>,
    opts: SendOptions,
    sink: &mut dyn ProgressSink,
) -> Result<SendSummary> {
    if items.is_empty() {
        return Err(Error::Config("no files to send".into()));
    }

    let transfer_id = opts
        .transfer_id
        .clone()
        .unwrap_or_else(generate_transfer_id);

    // Stat sources and build the offer. Capture (size, mtime) now; the same
    // values are re-checked just before streaming each file.
    let mut offers = Vec::with_capacity(items.len());
    let mut sender_files = Vec::with_capacity(items.len());
    let mut total_bytes: u64 = 0;
    for item in &items {
        let meta = std::fs::metadata(&item.source_path)?;
        if !meta.is_file() {
            return Err(Error::Config(format!(
                "not a regular file: {}",
                item.source_path.display()
            )));
        }
        let size = meta.len();
        let chunk_size = opts
            .chunk_size
            .unwrap_or_else(|| chunk::choose_chunk_size(size));
        let mtime = mtime_unix(&meta);
        let mode = unix_mode(&meta);

        offers.push(FileOffer {
            rel_path: item.rel_path.clone(),
            size,
            chunk_size,
            mode,
            mtime,
            content_hash: None,
        });
        sender_files.push(SenderFile {
            rel_path: item.rel_path.clone(),
            source_path: item.source_path.clone(),
            size,
            chunk_size,
        });
        total_bytes += size;
    }

    // Persist the sender-side record up front (state: in progress).
    let mut record = SenderRecord::new(
        transfer_id.clone(),
        target_addr.to_string(),
        common_root(&items),
        sender_files,
        manifest::now_unix(),
    );
    record.state = TransferState::InProgress;
    record.save_to(&config.transfers_dir())?;

    let result = run_send(
        config,
        identity,
        target_addr,
        &transfer_id,
        &items,
        &offers,
        total_bytes,
        sink,
    );

    match &result {
        Ok(_) => record.set_state_and_save(TransferState::Completed)?,
        Err(_) => record.set_state_and_save(TransferState::Failed)?,
    }
    result
}

/// Connect to `target_addr`, perform the Noise handshake, and exchange
/// Hello/HelloAck. Shared by the push and sync paths. Returns the authenticated
/// [`SecureStream`] ready for an `Offer` or `SyncOffer`.
pub(crate) fn connect_and_hello(
    config: &Config,
    identity: &DeviceIdentity,
    target_addr: &str,
    sink: &mut dyn ProgressSink,
) -> Result<SecureStream> {
    let stream = TcpStream::connect(target_addr)?;
    stream.set_nodelay(true).ok();
    let mut secure = transport::handshake_initiator(stream, identity)?;

    // Note (non-fatal) whether we already trust the receiver's key.
    let peers = config.load_peers()?;
    if !peers.is_authorized(secure.remote_fingerprint()) {
        sink.on_event(ProgressEvent::Note(format!(
            "note: receiver fingerprint {} is not in your paired list",
            secure.remote_fingerprint().short()
        )));
    }

    protocol::write_control(
        &mut secure,
        &Control::Hello {
            protocol_version: protocol::PROTOCOL_VERSION,
            device_name: identity.name.clone(),
            fingerprint: identity.fingerprint().full().to_string(),
        },
    )?;
    match protocol::read_control(&mut secure, config.limits.max_message_bytes)? {
        Control::HelloAck { accepted: true, .. } => {}
        Control::HelloAck {
            accepted: false,
            reason,
            ..
        } => return Err(Error::RejectedByPeer(reason)),
        other => return Err(Error::protocol(format!("expected HelloAck, got {other:?}"))),
    }
    Ok(secure)
}

#[allow(clippy::too_many_arguments)]
fn run_send(
    config: &Config,
    identity: &DeviceIdentity,
    target_addr: &str,
    transfer_id: &str,
    items: &[SendItem],
    offers: &[FileOffer],
    total_bytes: u64,
    sink: &mut dyn ProgressSink,
) -> Result<SendSummary> {
    let mut secure = connect_and_hello(config, identity, target_addr, sink)?;

    // Offer / OfferResponse.
    protocol::write_control(
        &mut secure,
        &Control::Offer {
            transfer_id: transfer_id.to_string(),
            files: offers.to_vec(),
        },
    )?;
    let plans = match protocol::read_control(&mut secure, config.limits.max_message_bytes)? {
        Control::OfferResponse { files } => files,
        other => {
            return Err(Error::protocol(format!(
                "expected OfferResponse, got {other:?}"
            )))
        }
    };

    sink.on_event(ProgressEvent::TransferStarted {
        transfer_id: transfer_id.to_string(),
        total_files: items.len() as u64,
        total_bytes,
    });

    let mut bytes_sent: u64 = 0;
    let mut bytes_skipped: u64 = 0;

    for (idx, item) in items.iter().enumerate() {
        let file_index = idx as u32;
        let offer = &offers[idx];
        let plan = plans
            .iter()
            .find(|p| p.file_index == file_index)
            .cloned()
            .unwrap_or(FilePlan {
                file_index,
                needed: ChunkSelection::All,
                resume: false,
            });

        let (sent, skipped) = send_one_file(config, &mut secure, item, offer, &plan, sink)?;
        bytes_sent += sent;
        bytes_skipped += skipped;
    }

    protocol::write_control(&mut secure, &Control::Complete)?;

    sink.on_event(ProgressEvent::TransferCompleted {
        transfer_id: transfer_id.to_string(),
        bytes_transferred: bytes_sent,
        bytes_skipped,
    });

    Ok(SendSummary {
        transfer_id: transfer_id.to_string(),
        files: items.len() as u64,
        bytes_sent,
        bytes_skipped,
    })
}

/// Stream one file: single sequential pass that computes the whole-file hash
/// and sends exactly the chunks named in `plan`. Returns `(bytes_sent,
/// bytes_skipped)`.
pub(crate) fn send_one_file(
    config: &Config,
    secure: &mut SecureStream,
    item: &SendItem,
    offer: &FileOffer,
    plan: &FilePlan,
    sink: &mut dyn ProgressSink,
) -> Result<(u64, u64)> {
    // Detect source changes: re-stat right before reading.
    let meta = std::fs::metadata(&item.source_path)?;
    if meta.len() != offer.size || mtime_unix(&meta) != offer.mtime {
        return Err(Error::SourceChanged(item.source_path.clone()));
    }

    let needed: Option<HashSet<u64>> = match &plan.needed {
        ChunkSelection::All => None, // None => send everything
        ChunkSelection::None => Some(HashSet::new()),
        ChunkSelection::Indices(v) => Some(v.iter().copied().collect()),
    };

    let count = chunk::chunk_count(offer.size, offer.chunk_size);
    let resumed_chunks = match &needed {
        Some(set) => count.saturating_sub(set.len() as u64),
        None => 0,
    };
    sink.on_event(ProgressEvent::FileStarted {
        file_index: plan.file_index,
        rel_path: item.rel_path.clone(),
        size: offer.size,
        resumed_bytes: resumed_chunks.saturating_mul(offer.chunk_size as u64),
    });

    let mut file = std::fs::File::open(&item.source_path)?;
    let mut hasher = chunk::FileHasher::new();
    let mut buf = vec![0u8; offer.chunk_size as usize];
    let mut chunk_index: u64 = 0;
    let mut bytes_sent: u64 = 0;
    let mut bytes_skipped: u64 = 0;

    loop {
        let n = read_fill(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        let data = &buf[..n];
        // Always fold into the whole-file hash so resume still verifies.
        hasher.update(data);

        let send_this = match &needed {
            None => true,
            Some(set) => set.contains(&chunk_index),
        };

        if send_this {
            let offset = chunk_index * offer.chunk_size as u64;
            let hash = chunk::hash_chunk(data);
            protocol::write_control(
                secure,
                &Control::ChunkHeader {
                    file_index: plan.file_index,
                    chunk_index,
                    offset,
                    len: n as u32,
                    hash,
                },
            )?;
            secure.write_all(data)?;
            bytes_sent += n as u64;
            sink.on_event(ProgressEvent::Progress {
                file_index: plan.file_index,
                delta_bytes: n as u64,
            });
        } else {
            bytes_skipped += n as u64;
        }

        chunk_index += 1;
        // Guard against the file growing beyond its declared size mid-pass.
        if chunk_index > count {
            return Err(Error::SourceChanged(item.source_path.clone()));
        }
    }
    secure.flush()?;

    let file_hash = hasher.finalize_hex();
    protocol::write_control(
        secure,
        &Control::FileDone {
            file_index: plan.file_index,
            file_hash,
        },
    )?;

    // Await the receiver's verdict for this file.
    match protocol::read_control(secure, config.limits.max_message_bytes)? {
        Control::FileResult { ok: true, .. } => {
            sink.on_event(ProgressEvent::FileCompleted {
                file_index: plan.file_index,
                rel_path: item.rel_path.clone(),
            });
        }
        Control::FileResult {
            ok: false, error, ..
        } => {
            return Err(Error::Integrity(format!(
                "receiver rejected '{}': {error}",
                item.rel_path
            )));
        }
        other => {
            return Err(Error::protocol(format!(
                "expected FileResult, got {other:?}"
            )))
        }
    }

    Ok((bytes_sent, bytes_skipped))
}

/// Read until `buf` is full or EOF; returns the number of bytes read.
fn read_fill<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Generate a random 128-bit transfer id as hex.
fn generate_transfer_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Longest common ancestor directory of the sources (for the sender record).
fn common_root(items: &[SendItem]) -> PathBuf {
    items
        .first()
        .and_then(|i| i.source_path.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn mtime_unix(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

#[cfg(unix)]
pub(crate) fn unix_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.mode())
}

#[cfg(not(unix))]
pub(crate) fn unix_mode(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Resume an existing transfer by id: re-reads the sender record and re-sends,
/// relying on the receiver's manifest to request only missing chunks.
pub fn resume(
    config: &Config,
    identity: &DeviceIdentity,
    transfer_id: &str,
    device_name: &str,
    sink: &mut dyn ProgressSink,
) -> Result<SendSummary> {
    let record = SenderRecord::load(&config.transfers_dir(), transfer_id)?
        .ok_or_else(|| Error::TransferNotFound(transfer_id.to_string()))?;
    let items = record
        .files
        .iter()
        .map(|f| SendItem {
            rel_path: f.rel_path.clone(),
            source_path: f.source_path.clone(),
        })
        .collect::<Vec<_>>();
    send(
        config,
        identity,
        &record.target_addr,
        items,
        SendOptions {
            transfer_id: Some(transfer_id.to_string()),
            chunk_size: None,
            device_name: device_name.to_string(),
        },
        sink,
    )
}
