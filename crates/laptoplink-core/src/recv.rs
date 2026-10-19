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
use crate::delta;
use crate::error::{Error, Result};
use crate::fsutil;
use crate::identity::DeviceIdentity;
use crate::manifest::{Bitmap, ReceiverFile, ReceiverManifest};
use crate::pathsafety;
use crate::progress::{ProgressEvent, ProgressSink};
use crate::protocol::{self, ChunkSelection, Control, FilePlan};
use crate::transport;
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

    // --- Dispatch: a push Offer or a one-way SyncOffer -------------------
    let first = protocol::read_control(&mut secure, config.limits.max_message_bytes)?;
    match first {
        Control::Offer { transfer_id, files } => {
            handle_offer(config, &mut secure, opts, sink, &fp, transfer_id, files)
        }
        Control::SyncOffer {
            sync_id,
            files,
            delete,
            dry_run,
            checksum,
            delta,
            excludes,
        } => handle_sync(
            config,
            &mut secure,
            opts,
            sink,
            &fp,
            sync_id,
            files,
            delete,
            dry_run,
            checksum,
            delta,
            excludes,
        ),
        other => Err(Error::protocol(format!(
            "expected Offer or SyncOffer, got {other:?}"
        ))),
    }
}

/// Validate coarse resource limits for an offered file set before allocating.
fn check_limits(files: &[protocol::FileOffer], limits: &crate::config::Limits) -> Result<()> {
    if files.len() as u64 > limits.max_files {
        return Err(Error::LimitExceeded(format!(
            "{} files exceeds limit {}",
            files.len(),
            limits.max_files
        )));
    }
    let mut total_chunks: u64 = 0;
    for f in files {
        if f.chunk_size as usize > limits.max_chunk_bytes || f.chunk_size == 0 {
            return Err(Error::LimitExceeded(format!(
                "chunk size {} out of bounds for '{}'",
                f.chunk_size, f.rel_path
            )));
        }
        total_chunks = total_chunks.saturating_add(chunk::chunk_count(f.size, f.chunk_size));
    }
    if total_chunks > limits.max_total_chunks {
        return Err(Error::LimitExceeded(format!(
            "{total_chunks} total chunks exceeds limit {}",
            limits.max_total_chunks
        )));
    }
    Ok(())
}

/// Handle a push transfer ([`Control::Offer`]): manifest-based resume planning,
/// then the shared receive loop.
#[allow(clippy::too_many_arguments)]
fn handle_offer(
    config: &Config,
    secure: &mut transport::SecureStream,
    opts: &ReceiveOptions,
    sink: &mut dyn ProgressSink,
    fp: &crate::identity::Fingerprint,
    transfer_id: String,
    files: Vec<protocol::FileOffer>,
) -> Result<()> {
    check_limits(&files, &config.limits)?;

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
                let _ = std::fs::remove_file(&part_path);
                (Bitmap::new(count), false, false)
            }
        } else {
            (Bitmap::new(count), false, false)
        };

        upsert_file(
            &mut manifest,
            ReceiverFile {
                file_index,
                rel_path: f.rel_path.clone(),
                size: f.size,
                chunk_size: f.chunk_size,
                src_mtime: f.mtime,
                expected_file_hash: None,
                verified: verified.clone(),
                part_path,
                committed: already_committed,
            },
        );

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
            delta: false,
        });
    }
    manifest.save()?;

    protocol::write_control(secure, &Control::OfferResponse { files: plans })?;

    let bytes = run_receive_loop(config, secure, &opts.dest, &mut manifest, sink)?;
    manifest.save()?;
    sink.on_event(ProgressEvent::TransferCompleted {
        transfer_id,
        bytes_transferred: bytes,
        bytes_skipped: total_bytes.saturating_sub(bytes),
    });
    Ok(())
}

/// Handle a one-way sync ([`Control::SyncOffer`]): compare against the real
/// destination tree, skip identical files, report/perform deletions, and
/// transfer changed/new files via the shared receive loop.
#[allow(clippy::too_many_arguments)]
fn handle_sync(
    config: &Config,
    secure: &mut transport::SecureStream,
    opts: &ReceiveOptions,
    sink: &mut dyn ProgressSink,
    fp: &crate::identity::Fingerprint,
    sync_id: String,
    files: Vec<protocol::FileOffer>,
    delete: bool,
    dry_run: bool,
    checksum: bool,
    delta_enabled: bool,
    excludes: Vec<String>,
) -> Result<()> {
    check_limits(&files, &config.limits)?;

    let source_set: HashSet<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
    let tmp_dir = opts.dest.join(".laptoplink").join("tmp").join(&sync_id);
    let mut manifest = ReceiverManifest::new(&opts.dest, &sync_id, fp.full());
    let mut plans = Vec::with_capacity(files.len());
    let mut conflicts = Vec::new();
    let mut transfer_bytes_total: u64 = 0;

    for (idx, f) in files.iter().enumerate() {
        let file_index = idx as u32;
        let dest_path = pathsafety::safe_join(&opts.dest, &f.rel_path)?;
        let count = chunk::chunk_count(f.size, f.chunk_size);

        // Compare against the actual destination file to decide skip vs send,
        // and whether a changed-but-present file is eligible for delta.
        let (needed, use_delta) = match std::fs::symlink_metadata(&dest_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                conflicts.push(format!("{}: destination is a symlink; skipped", f.rel_path));
                (ChunkSelection::None, false)
            }
            Ok(meta) if meta.is_dir() => {
                conflicts.push(format!(
                    "{}: destination is a directory; skipped",
                    f.rel_path
                ));
                (ChunkSelection::None, false)
            }
            Ok(meta) if meta.is_file() => {
                if files_identical(&dest_path, &meta, f, checksum) {
                    (ChunkSelection::None, false)
                } else {
                    // Exists but differs: eligible for delta if enabled and
                    // the file is large enough to be worth signing.
                    let d = delta_enabled && f.size >= delta::MIN_DELTA_FILE;
                    (ChunkSelection::All, d)
                }
            }
            _ => (ChunkSelection::All, false), // not found -> new file, no basis
        };

        if matches!(needed, ChunkSelection::All) {
            transfer_bytes_total += f.size;
            if !dry_run {
                upsert_file(
                    &mut manifest,
                    ReceiverFile {
                        file_index,
                        rel_path: f.rel_path.clone(),
                        size: f.size,
                        chunk_size: f.chunk_size,
                        src_mtime: f.mtime,
                        expected_file_hash: None,
                        verified: Bitmap::new(count),
                        part_path: tmp_dir.join(format!("{file_index}.part")),
                        committed: false,
                    },
                );
            }
        }

        plans.push(FilePlan {
            file_index,
            needed,
            resume: false,
            delta: use_delta,
        });
    }

    // Compute extraneous destination files (candidates for deletion).
    let deletions = scan_deletions(&opts.dest, &source_set, &excludes);

    sink.on_event(ProgressEvent::TransferStarted {
        transfer_id: sync_id.clone(),
        total_files: files.len() as u64,
        total_bytes: transfer_bytes_total,
    });

    protocol::write_control(
        secure,
        &Control::SyncResponse {
            files: plans,
            deletions: deletions.clone(),
            conflicts,
        },
    )?;

    if dry_run {
        // No changes on either side; just drain the sender's Complete.
        match protocol::read_control(secure, config.limits.max_message_bytes)? {
            Control::Complete => {}
            Control::Error { message } => {
                return Err(Error::protocol(format!("sender aborted: {message}")))
            }
            other => return Err(Error::protocol(format!("expected Complete, got {other:?}"))),
        }
        protocol::write_control(secure, &Control::SyncComplete { deleted: 0 })?;
        secure.graceful_close();
        return Ok(());
    }

    std::fs::create_dir_all(&tmp_dir)?;
    manifest.save()?;

    let bytes = run_receive_loop(config, secure, &opts.dest, &mut manifest, sink)?;
    manifest.save()?;

    // Deletions happen only after all transfers succeeded, and only if asked.
    let mut deleted = 0u64;
    if delete {
        for rel in &deletions {
            let path = pathsafety::safe_join(&opts.dest, rel)?;
            // Refuse to follow a symlink when deleting.
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                if meta.is_file() && std::fs::remove_file(&path).is_ok() {
                    deleted += 1;
                }
            }
        }
        prune_empty_dirs(&opts.dest);
    }

    // Clean up this sync's transient manifest + part dir.
    let _ = std::fs::remove_file(ReceiverManifest::manifest_path(&opts.dest, &sync_id));
    let _ = std::fs::remove_dir_all(&tmp_dir);

    protocol::write_control(secure, &Control::SyncComplete { deleted })?;
    sink.on_event(ProgressEvent::TransferCompleted {
        transfer_id: sync_id,
        bytes_transferred: bytes,
        bytes_skipped: transfer_bytes_total.saturating_sub(bytes),
    });
    secure.graceful_close();
    Ok(())
}

/// Is the destination file already identical to the offered source file?
/// By size + mtime (fast, default) or whole-file hash when `checksum`.
fn files_identical(
    dest_path: &Path,
    dest_meta: &std::fs::Metadata,
    offer: &protocol::FileOffer,
    checksum: bool,
) -> bool {
    if dest_meta.len() != offer.size {
        return false;
    }
    if checksum {
        match (&offer.content_hash, hash_whole_file(dest_path)) {
            (Some(expected), Ok(got)) => expected == &got,
            _ => false,
        }
    } else {
        // Require a usable mtime on both sides; otherwise re-send to be safe.
        match (mtime_secs(dest_meta), offer.mtime) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

/// Walk the destination tree and return relative paths that are not in the
/// source set and are not protected by an exclude pattern (and never anything
/// under the `.laptoplink` control directory).
fn scan_deletions(dest: &Path, source_set: &HashSet<&str>, excludes: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if !dest.exists() {
        return out;
    }
    for entry in walkdir::WalkDir::new(dest)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(dest) {
            Ok(r) => pathsafety::to_wire_rel(r),
            Err(_) => continue,
        };
        if rel.starts_with(".laptoplink") {
            continue;
        }
        if source_set.contains(rel.as_str()) {
            continue;
        }
        if crate::glob::any_match(excludes, &rel) {
            continue; // excluded files are protected from deletion
        }
        out.push(rel);
    }
    out.sort();
    out
}

/// Best-effort removal of directories left empty after deletions.
fn prune_empty_dirs(dest: &Path) {
    // Walk deepest-first so children are gone before we test a parent.
    let mut dirs: Vec<PathBuf> = walkdir::WalkDir::new(dest)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.path().to_path_buf())
        .collect();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for dir in dirs {
        if dir == *dest || dir.ends_with(".laptoplink") {
            continue;
        }
        if dir.starts_with(dest.join(".laptoplink")) {
            continue;
        }
        let _ = std::fs::remove_dir(&dir); // only succeeds if empty
    }
}

/// The shared chunk-receive loop used by both push and sync: reads chunk
/// headers + payloads, verifies each, writes to the part file, and finalizes
/// each file on `FileDone`. Returns the number of bytes written. Stops on
/// `Complete`.
fn run_receive_loop(
    config: &Config,
    secure: &mut transport::SecureStream,
    dest: &Path,
    manifest: &mut ReceiverManifest,
    sink: &mut dyn ProgressSink,
) -> Result<u64> {
    let mut payload = vec![0u8; 0];
    let mut bytes_written: u64 = 0;
    let mut open_file: Option<(u32, std::fs::File)> = None;
    let mut since_checkpoint: u64 = 0;
    let mut delta_recv: Option<DeltaRecv> = None;

    loop {
        let msg = protocol::read_control(secure, config.limits.max_message_bytes)?;
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
                if payload.len() < len as usize {
                    payload.resize(len as usize, 0);
                }
                read_exact(secure, &mut payload[..len as usize])?;

                // Verify integrity BEFORE touching disk.
                let got = chunk::hash_chunk(&payload[..len as usize]);
                if got != hash {
                    return Err(Error::Integrity(format!(
                        "chunk {chunk_index} of file {file_index}: hash mismatch"
                    )));
                }

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

                rf.verified.set(chunk_index);
                bytes_written += len as u64;
                since_checkpoint += len as u64;
                sink.on_event(ProgressEvent::Progress {
                    file_index,
                    delta_bytes: len as u64,
                });

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
                if open_file.as_ref().map(|(i, _)| *i) == Some(file_index) {
                    if let Some((_, f)) = open_file.take() {
                        fsutil::sync_file(&f)?;
                    }
                }
                let result = finalize_file(dest, manifest, file_index, &file_hash);
                manifest.save()?;
                match result {
                    Ok(rel_path) => {
                        protocol::write_control(
                            secure,
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
                            secure,
                            &Control::FileResult {
                                file_index,
                                ok: false,
                                error: e.to_string(),
                            },
                        )?;
                    }
                }
            }

            // --- Delta transfer (basis = existing destination file) --------
            Control::DeltaRequest { file_index } => {
                let rf = manifest
                    .file_mut(file_index)
                    .ok_or_else(|| Error::protocol("DeltaRequest for unknown file index"))?;
                let rel = rf.rel_path.clone();
                let part_path = rf.part_path.clone();
                let basis_path = pathsafety::safe_join(dest, &rel)?;
                let basis_len = std::fs::metadata(&basis_path)?.len();
                let block_size = delta::block_size_for(basis_len);
                let sigs = delta::signatures_file(&basis_path, block_size)?;
                let wire: Vec<(u32, String)> =
                    sigs.into_iter().map(|s| (s.weak, s.strong)).collect();
                protocol::write_control(
                    secure,
                    &Control::DeltaSignatures {
                        file_index,
                        block_size,
                        sigs: wire,
                    },
                )?;
                // Close any open chunk file; open basis (read) + fresh part.
                if let Some((_, f)) = open_file.take() {
                    fsutil::sync_file(&f)?;
                }
                let basis = std::fs::File::open(&basis_path)?;
                let part = fsutil::create_part_file(&part_path)?;
                delta_recv = Some(DeltaRecv {
                    file_index,
                    basis,
                    part,
                    part_path,
                    block_size,
                });
            }

            Control::DeltaCopy {
                file_index,
                start_block,
                count,
            } => {
                let d = delta_recv
                    .as_mut()
                    .filter(|d| d.file_index == file_index)
                    .ok_or_else(|| Error::protocol("DeltaCopy without matching DeltaRequest"))?;
                let bs = d.block_size as u64;
                d.basis
                    .seek(SeekFrom::Start(start_block.saturating_mul(bs)))?;
                let mut buf = vec![0u8; d.block_size as usize];
                for _ in 0..count {
                    let n = read_up_to(&mut d.basis, &mut buf)?;
                    if n == 0 {
                        return Err(Error::Integrity("delta copy past end of basis".into()));
                    }
                    d.part.write_all(&buf[..n])?;
                }
            }

            Control::DeltaLiteral { file_index, len } => {
                if len as usize > config.limits.max_chunk_bytes {
                    return Err(Error::LimitExceeded(format!(
                        "delta literal {len} too large"
                    )));
                }
                let d = delta_recv
                    .as_mut()
                    .filter(|d| d.file_index == file_index)
                    .ok_or_else(|| Error::protocol("DeltaLiteral without matching DeltaRequest"))?;
                if payload.len() < len as usize {
                    payload.resize(len as usize, 0);
                }
                read_exact(secure, &mut payload[..len as usize])?;
                d.part.write_all(&payload[..len as usize])?;
                bytes_written += len as u64;
                sink.on_event(ProgressEvent::Progress {
                    file_index,
                    delta_bytes: len as u64,
                });
            }

            Control::DeltaDone {
                file_index,
                file_hash,
            } => {
                let d = delta_recv
                    .take()
                    .filter(|d| d.file_index == file_index)
                    .ok_or_else(|| Error::protocol("DeltaDone without matching DeltaRequest"))?;
                d.part.sync_all()?;
                drop(d.basis);
                let result = finalize_delta(dest, manifest, file_index, &d.part_path, &file_hash);
                manifest.save()?;
                match result {
                    Ok(rel_path) => {
                        protocol::write_control(
                            secure,
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
                            secure,
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

    Ok(bytes_written)
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

    // Preserve the source mtime so a later `sync` can skip this file by its
    // size + mtime signature instead of re-transferring it every run.
    if let Some(secs) = rf.src_mtime {
        if let Ok(f) = std::fs::File::options().write(true).open(&dest_path) {
            let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(secs));
        }
    }

    rf.committed = true;
    Ok(rf.rel_path.clone())
}

/// In-flight state while reconstructing one file from a delta stream.
struct DeltaRecv {
    file_index: u32,
    /// The existing destination file, read as the copy basis.
    basis: std::fs::File,
    /// The new file being assembled.
    part: std::fs::File,
    part_path: PathBuf,
    block_size: u32,
}

/// Finalize a delta-reconstructed file: verify its whole-file hash, then
/// atomically commit (symlink-guarded, mtime-preserving). On mismatch the part
/// file is discarded so a retry starts clean.
fn finalize_delta(
    dest_root: &Path,
    manifest: &mut ReceiverManifest,
    file_index: u32,
    part_path: &Path,
    expected_hash: &str,
) -> Result<String> {
    let rf = manifest
        .file_mut(file_index)
        .ok_or_else(|| Error::protocol("DeltaDone for unknown file index"))?;
    rf.expected_file_hash = Some(expected_hash.to_string());

    let got = hash_whole_file(part_path)?;
    if got != expected_hash {
        let _ = std::fs::remove_file(part_path);
        return Err(Error::Integrity(format!(
            "delta reconstruction hash mismatch for '{}'",
            rf.rel_path
        )));
    }

    let dest_path = pathsafety::safe_join(dest_root, &rf.rel_path)?;
    guard_symlink(&dest_path)?;
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    fsutil::atomic_rename(part_path, &dest_path)?;
    if let Some(secs) = rf.src_mtime {
        if let Ok(f) = std::fs::File::options().write(true).open(&dest_path) {
            let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(secs));
        }
    }
    rf.committed = true;
    Ok(rf.rel_path.clone())
}

/// Read up to `buf.len()` bytes; returns bytes read (short only at EOF).
fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
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

/// A file's mtime in whole Unix seconds, if the platform/filesystem reports it.
fn mtime_secs(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Stream a file and return its hex BLAKE3 hash (bounded memory).
fn hash_whole_file(path: &Path) -> Result<String> {
    let mut hasher = chunk::FileHasher::new();
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize_hex())
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
