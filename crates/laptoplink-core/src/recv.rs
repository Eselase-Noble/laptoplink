//! The receiving (responder) side of a transfer.
//!
//! Responsibilities:
//! * Accept TCP connections and perform the Noise handshake.
//! * Authorize the peer by its static-key fingerprint against the peer store
//!   (or pair it on first contact when `accept_new` is set).
//! * Plan each offered file against a durable manifest to enable resume.
//! * Receive chunks, verify each by BLAKE3 hash, and write to a `.part` file.
//! * Verify the whole-file hash, then atomically commit to the destination.
//!
//! Memory use is bounded: at most one chunk payload is buffered at a time.

use crate::chunk;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::fsutil;
use crate::identity::DeviceIdentity;
use crate::manifest::{Bitmap, ReceiverFile, ReceiverManifest};
use crate::pathsafety;
use crate::progress::{ProgressEvent, ProgressSink};
use crate::protocol::{self, ChunkSelection, Control, FilePlan};
use crate::transport;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

/// Options controlling the receive server.
pub struct ReceiveOptions {
    /// Directory under which received files are written.
    pub dest: PathBuf,
    /// Trust-on-first-use pairing: approve and persist unknown peers instead of
    /// rejecting them. Off by default — unknown peers are refused.
    pub accept_new: bool,
    /// This device's display name, sent back in `HelloAck`.
    pub device_name: String,
    /// Handle exactly one connection then return (used by tests/one-shot mode).
    pub once: bool,
}

/// Bind `bind_addr` and serve inbound transfers until `once` is satisfied or an
/// unrecoverable error occurs. Each connection is handled sequentially in this
/// phase (see `docs/FEATURES.md` for concurrent-receive as remaining work).
pub fn listen(
    config: &Config,
    identity: &DeviceIdentity,
    bind_addr: &str,
    opts: &ReceiveOptions,
    sink: &mut dyn ProgressSink,
) -> Result<()> {
    let listener = TcpListener::bind(bind_addr)?;
    sink.on_event(ProgressEvent::Note(format!(
        "listening on {} as '{}' (fingerprint {})",
        listener.local_addr()?,
        opts.device_name,
        identity.fingerprint().short()
    )));

    loop {
        let (stream, peer) = listener.accept()?;
        // One misbehaving connection must not take down the server.
        if let Err(e) = handle_connection(config, identity, stream, opts, sink) {
            sink.on_event(ProgressEvent::Note(format!(
                "connection from {peer} ended: {e}"
            )));
        }
        if opts.once {
            break;
        }
    }
    Ok(())
}

/// The local address a bound listener actually chose (useful when binding to
/// port 0 in tests). Returns a listener the caller then passes to [`serve_on`].
pub fn bind(bind_addr: &str) -> Result<TcpListener> {
    Ok(TcpListener::bind(bind_addr)?)
}

/// Serve on an already-bound listener (lets tests learn the port first).
pub fn serve_on(
    listener: &TcpListener,
    config: &Config,
    identity: &DeviceIdentity,
    opts: &ReceiveOptions,
    sink: &mut dyn ProgressSink,
) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept()?;
        if let Err(e) = handle_connection(config, identity, stream, opts, sink) {
            sink.on_event(ProgressEvent::Note(format!(
                "connection from {peer} ended: {e}"
            )));
        }
        if opts.once {
            break;
        }
    }
    Ok(())
}

fn handle_connection(
    config: &Config,
    identity: &DeviceIdentity,
    stream: TcpStream,
    opts: &ReceiveOptions,
    sink: &mut dyn ProgressSink,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    let mut secure = transport::handshake_responder(stream, identity)?;
    let fp = secure.remote_fingerprint().clone();

    // --- Hello / authorization -------------------------------------------
    let hello = protocol::read_control(&mut secure, config.limits.max_message_bytes)?;
    let peer_name = match &hello {
        Control::Hello {
            protocol_version,
            device_name,
            ..
        } => {
            if *protocol_version != protocol::PROTOCOL_VERSION {
                let _ = protocol::write_control(
                    &mut secure,
                    &Control::HelloAck {
                        accepted: false,
                        reason: format!(
                            "protocol version mismatch (mine {}, yours {})",
                            protocol::PROTOCOL_VERSION,
                            protocol_version
                        ),
                        device_name: opts.device_name.clone(),
                    },
                );
                return Err(Error::VersionMismatch {
                    local: protocol::PROTOCOL_VERSION,
                    remote: *protocol_version,
                });
            }
            device_name.clone()
        }
        other => return Err(Error::protocol(format!("expected Hello, got {other:?}"))),
    };

    let mut peers = config.load_peers()?;
    if !peers.is_authorized(&fp) {
        if opts.accept_new {
            peers.approve(&fp, &peer_name)?;
            sink.on_event(ProgressEvent::Note(format!(
                "paired new device '{peer_name}' (fingerprint {})",
                fp.short()
            )));
        } else {
            let _ = protocol::write_control(
                &mut secure,
                &Control::HelloAck {
                    accepted: false,
                    reason: "device not paired; run `laptoplink device pair` or restart the \
                             receiver with --accept-new"
                        .into(),
                    device_name: opts.device_name.clone(),
                },
            );
            return Err(Error::PeerNotAuthorized(fp.short()));
        }
    }

    protocol::write_control(
        &mut secure,
        &Control::HelloAck {
            accepted: true,
            reason: String::new(),
            device_name: opts.device_name.clone(),
        },
    )?;

    // --- Offer / plan -----------------------------------------------------
    let offer = protocol::read_control(&mut secure, config.limits.max_message_bytes)?;
    let (transfer_id, files) = match offer {
        Control::Offer { transfer_id, files } => (transfer_id, files),
        other => return Err(Error::protocol(format!("expected Offer, got {other:?}"))),
    };

    // Enforce coarse resource limits before allocating anything per-file.
    if files.len() as u64 > config.limits.max_files {
        return Err(Error::LimitExceeded(format!(
            "{} files exceeds limit {}",
            files.len(),
            config.limits.max_files
        )));
    }
    let mut total_chunks: u64 = 0;
    for f in &files {
        if f.chunk_size as usize > config.limits.max_chunk_bytes || f.chunk_size == 0 {
            return Err(Error::LimitExceeded(format!(
                "chunk size {} out of bounds for '{}'",
                f.chunk_size, f.rel_path
            )));
        }
        total_chunks = total_chunks.saturating_add(chunk::chunk_count(f.size, f.chunk_size));
    }
    if total_chunks > config.limits.max_total_chunks {
        return Err(Error::LimitExceeded(format!(
            "{total_chunks} total chunks exceeds limit {}",
            config.limits.max_total_chunks
        )));
    }

    // Load or create the durable manifest and derive each file's plan.
    let mut manifest = ReceiverManifest::load(&opts.dest, &transfer_id)?
        .unwrap_or_else(|| ReceiverManifest::new(&opts.dest, &transfer_id, fp.full()));

    let tmp_dir = opts.dest.join(".laptoplink").join("tmp").join(&transfer_id);
    std::fs::create_dir_all(&tmp_dir)?;

    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    sink.on_event(ProgressEvent::TransferStarted {
        transfer_id: transfer_id.clone(),
        total_files: files.len() as u64,
        total_bytes,
    });

    let mut plans = Vec::with_capacity(files.len());
    for (idx, f) in files.iter().enumerate() {
        let file_index = idx as u32;
        // Validate the destination path before trusting it anywhere else.
        let dest_path = pathsafety::safe_join(&opts.dest, &f.rel_path)?;
        let part_path = tmp_dir.join(format!("{file_index}.part"));

        let count = chunk::chunk_count(f.size, f.chunk_size);

        // Decide whether prior partial data is still valid for resume.
        let existing = manifest.files.iter().find(|rf| rf.rel_path == f.rel_path);
        let reuse = existing
            .map(|rf| {
                !rf.committed
                    && rf.size == f.size
                    && rf.chunk_size == f.chunk_size
                    && rf.src_mtime == f.mtime
                    && rf.verified.len() == count
            })
            .unwrap_or(false);

        let (verified, resume, already_committed) = if let Some(rf) = existing {
            if rf.committed && dest_path.exists() {
                (Bitmap::new(count), false, true)
            } else if reuse {
                (rf.verified.clone(), rf.verified.count_set() > 0, false)
            } else {
                // Signature changed or never started: discard any partial data.
                let _ = std::fs::remove_file(&part_path);
                (Bitmap::new(count), false, false)
            }
        } else {
            (Bitmap::new(count), false, false)
        };

        // Record (or refresh) this file's manifest entry.
        let entry = ReceiverFile {
            file_index,
            rel_path: f.rel_path.clone(),
            size: f.size,
            chunk_size: f.chunk_size,
            src_mtime: f.mtime,
            expected_file_hash: None,
            verified: verified.clone(),
            part_path: part_path.clone(),
            committed: already_committed,
        };
        upsert_file(&mut manifest, entry);

        let needed = if already_committed {
            ChunkSelection::None
        } else {
            let missing = verified.missing();
            if missing.len() as u64 == count {
                ChunkSelection::All
            } else if missing.is_empty() {
                ChunkSelection::None
            } else {
                ChunkSelection::Indices(missing)
            }
        };

        plans.push(FilePlan {
            file_index,
            needed,
            resume,
        });
    }
    manifest.save()?;

    protocol::write_control(&mut secure, &Control::OfferResponse { files: plans })?;

    // --- Receive loop -----------------------------------------------------
    let mut payload = vec![0u8; 0];
    let mut bytes_written: u64 = 0;
    let mut open_file: Option<(u32, std::fs::File)> = None;
    let mut since_checkpoint: u64 = 0;

    loop {
        let msg = protocol::read_control(&mut secure, config.limits.max_message_bytes)?;
        match msg {
            Control::ChunkHeader {
                file_index,
                chunk_index,
                offset,
                len,
                hash,
            } => {
                if len as usize > config.limits.max_chunk_bytes {
                    return Err(Error::LimitExceeded(format!("chunk len {len} too large")));
                }
                // Read exactly `len` bytes of (decrypted) payload.
                if payload.len() < len as usize {
                    payload.resize(len as usize, 0);
                }
                read_exact(&mut secure, &mut payload[..len as usize])?;

                // Verify integrity BEFORE touching disk.
                let got = chunk::hash_chunk(&payload[..len as usize]);
                if got != hash {
                    return Err(Error::Integrity(format!(
                        "chunk {chunk_index} of file {file_index}: hash mismatch"
                    )));
                }

                // Open the right part file (keep the last one open across chunks).
                let rf = manifest
                    .file_mut(file_index)
                    .ok_or_else(|| Error::protocol("chunk for unknown file index"))?;
                let part_path = rf.part_path.clone();
                if open_file.as_ref().map(|(i, _)| *i) != Some(file_index) {
                    if let Some((_, f)) = open_file.take() {
                        fsutil::sync_file(&f)?;
                    }
                    let f = fsutil::open_part_file(&part_path)?;
                    open_file = Some((file_index, f));
                }
                let f = &mut open_file.as_mut().unwrap().1;
                f.seek(SeekFrom::Start(offset))?;
                f.write_all(&payload[..len as usize])?;

                // Mark verified only after the write returns.
                rf.verified.set(chunk_index);
                bytes_written += len as u64;
                since_checkpoint += len as u64;
                sink.on_event(ProgressEvent::Progress {
                    file_index,
                    delta_bytes: len as u64,
                });

                // Periodic durable checkpoint so resume works after a crash.
                if since_checkpoint >= config.limits.max_inflight_bytes as u64 {
                    if let Some((_, f)) = open_file.as_ref() {
                        fsutil::sync_file(f)?;
                    }
                    manifest.save()?;
                    since_checkpoint = 0;
                }
            }

            Control::FileDone {
                file_index,
                file_hash,
            } => {
                // Flush and close the part file before verifying + committing.
                if open_file.as_ref().map(|(i, _)| *i) == Some(file_index) {
                    if let Some((_, f)) = open_file.take() {
                        fsutil::sync_file(&f)?;
                    }
                }
                let result = finalize_file(&opts.dest, &mut manifest, file_index, &file_hash);
                manifest.save()?;
                match result {
                    Ok(rel_path) => {
                        protocol::write_control(
                            &mut secure,
                            &Control::FileResult {
                                file_index,
                                ok: true,
                                error: String::new(),
                            },
                        )?;
                        sink.on_event(ProgressEvent::FileCompleted {
                            file_index,
                            rel_path,
                        });
                    }
                    Err(e) => {
                        protocol::write_control(
                            &mut secure,
                            &Control::FileResult {
                                file_index,
                                ok: false,
                                error: e.to_string(),
                            },
                        )?;
                    }
                }
            }

            Control::Complete => break,
            Control::Error { message } => {
                return Err(Error::protocol(format!("sender aborted: {message}")));
            }
            other => return Err(Error::protocol(format!("unexpected message: {other:?}"))),
        }
    }

    manifest.save()?;
    sink.on_event(ProgressEvent::TransferCompleted {
        transfer_id,
        bytes_transferred: bytes_written,
        bytes_skipped: total_bytes.saturating_sub(bytes_written),
    });
    Ok(())
}

/// Verify a fully-received file against `expected_hash`, then atomically move it
/// into place. On hash mismatch the partial data is discarded so a retry starts
/// clean. Returns the file's relative path on success.
fn finalize_file(
    dest_root: &std::path::Path,
    manifest: &mut ReceiverManifest,
    file_index: u32,
    expected_hash: &str,
) -> Result<String> {
    let rf = manifest
        .file_mut(file_index)
        .ok_or_else(|| Error::protocol("FileDone for unknown file index"))?;

    // Already committed on a prior run (resume of a completed file): the
    // destination is in place and verified; nothing more to do.
    if rf.committed {
        return Ok(rf.rel_path.clone());
    }

    rf.expected_file_hash = Some(expected_hash.to_string());

    if !rf.verified.all_set() {
        return Err(Error::Integrity(format!(
            "file {} incomplete: {}/{} chunks",
            rf.rel_path,
            rf.verified.count_set(),
            rf.verified.len()
        )));
    }

    // Stream the part file to compute the whole-file hash (bounded memory).
    let mut hasher = chunk::FileHasher::new();
    {
        let mut f = std::fs::File::open(&rf.part_path)?;
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }
    let got = hasher.finalize_hex();
    if got != expected_hash {
        // Reconstruction is wrong (corruption or source changed mid-transfer).
        rf.verified = Bitmap::new(rf.verified.len());
        let _ = std::fs::remove_file(&rf.part_path);
        return Err(Error::Integrity(format!(
            "whole-file hash mismatch for '{}'",
            rf.rel_path
        )));
    }

    // Commit: refuse to clobber a symlink, create parents, atomic rename.
    let dest_path = pathsafety::safe_join(dest_root, &rf.rel_path)?;
    guard_symlink(&dest_path)?;
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    fsutil::atomic_rename(&rf.part_path, &dest_path)?;
    rf.committed = true;
    Ok(rf.rel_path.clone())
}

/// Reject commit if the destination already exists as a symlink — prevents a
/// malicious or stale link from redirecting our write outside the tree.
fn guard_symlink(dest_path: &std::path::Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(dest_path) {
        if meta.file_type().is_symlink() {
            return Err(Error::UnsafePath(format!(
                "destination '{}' is a symlink; refusing to overwrite",
                dest_path.display()
            )));
        }
    }
    Ok(())
}

/// Insert or replace a file entry by `file_index`.
fn upsert_file(manifest: &mut ReceiverManifest, entry: ReceiverFile) {
    if let Some(slot) = manifest
        .files
        .iter_mut()
        .find(|f| f.file_index == entry.file_index)
    {
        *slot = entry;
    } else {
        manifest.files.push(entry);
    }
}

/// Read exactly `buf.len()` bytes from a reader, erroring on early EOF.
fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            return Err(Error::protocol("unexpected EOF reading chunk payload"));
        }
        filled += n;
    }
    Ok(())
}
