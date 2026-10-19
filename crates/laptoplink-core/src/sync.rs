//! One-way directory synchronization (the sending side).
//!
//! `sync` makes the destination's contents match a source directory, moving
//! only what has changed. Semantics (close to `rsync -rt src/ dest/`):
//!
//! * The **contents** of the source directory are mapped under the destination
//!   root — the source directory name itself is not added as a prefix.
//! * A destination file is **skipped** when it already matches by size + mtime
//!   (or by content hash with `checksum`).
//! * New and changed files are **transferred** (full-file in this stage; delta
//!   transfer is layered on in a later step).
//! * With `delete`, destination files that no longer exist in the source are
//!   removed — but only after all transfers succeed, never by default, and
//!   never for paths protected by an exclude pattern.
//! * `dry_run` computes and reports the plan without changing anything.
//!
//! The actual comparison and any deletions happen on the **receiver**, which is
//! the only side that can see the destination tree; this module drives the
//! exchange and reports the outcome.

use crate::chunk;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::glob;
use crate::identity::DeviceIdentity;
use crate::pathsafety;
use crate::progress::{ProgressEvent, ProgressSink};
use crate::protocol::{self, ChunkSelection, Control, FileOffer};
use crate::send::{self, SendItem};
use std::io::Read;
use std::path::Path;

/// Options for a sync operation.
pub struct SyncOptions {
    /// Delete extraneous destination files (gated; off by default).
    pub delete: bool,
    /// Compute and report the plan without transferring or deleting.
    pub dry_run: bool,
    /// Compare file contents by hash instead of size + mtime.
    pub checksum: bool,
    /// Use delta transfer for changed files that already exist at the dest.
    pub delta: bool,
    /// Exclude globs applied to both the source list and deletion scan.
    pub excludes: Vec<String>,
    /// Force a chunk size for transferred files (else adaptive).
    pub chunk_size: Option<u32>,
    /// This device's display name.
    pub device_name: String,
}

/// Outcome of a sync, for reporting and exit-code decisions.
#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub dry_run: bool,
    pub files_total: u64,
    pub files_transferred: u64,
    pub files_skipped: u64,
    pub bytes_transferred: u64,
    /// Destination paths that are extraneous (deleted, or would be in dry-run).
    pub deletions: Vec<String>,
    /// Human-readable conflict notes from the receiver.
    pub conflicts: Vec<String>,
    /// Number of files actually deleted (0 in dry-run or without `delete`).
    pub deleted: u64,
}

/// Synchronize the contents of `source_dir` into `target_addr`'s destination.
pub fn sync(
    config: &Config,
    identity: &DeviceIdentity,
    target_addr: &str,
    source_dir: &Path,
    opts: SyncOptions,
    sink: &mut dyn ProgressSink,
) -> Result<SyncSummary> {
    let meta = std::fs::metadata(source_dir)
        .map_err(|e| Error::Config(format!("source '{}': {e}", source_dir.display())))?;
    if !meta.is_dir() {
        return Err(Error::Config(format!(
            "sync source must be a directory: {}",
            source_dir.display()
        )));
    }

    // Build the source file list (relative to the source root) and filter it.
    let mut items = expand_for_sync(source_dir)?;
    items.retain(|it| !glob::any_match(&opts.excludes, &it.rel_path));
    items.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    // Build the offer, optionally hashing contents for --checksum.
    let mut offers = Vec::with_capacity(items.len());
    for it in &items {
        let m = std::fs::metadata(&it.source_path)?;
        let size = m.len();
        let chunk_size = opts
            .chunk_size
            .unwrap_or_else(|| chunk::choose_chunk_size(size));
        let content_hash = if opts.checksum {
            Some(hash_whole_file(&it.source_path)?)
        } else {
            None
        };
        offers.push(FileOffer {
            rel_path: it.rel_path.clone(),
            size,
            chunk_size,
            mode: send::unix_mode(&m),
            mtime: send::mtime_unix(&m),
            content_hash,
        });
    }

    let mut secure = send::connect_and_hello(config, identity, target_addr, sink)?;

    protocol::write_control(
        &mut secure,
        &Control::SyncOffer {
            sync_id: generate_id(),
            files: offers.clone(),
            delete: opts.delete,
            dry_run: opts.dry_run,
            checksum: opts.checksum,
            delta: opts.delta,
            excludes: opts.excludes.clone(),
        },
    )?;

    // The receiver's plan: per-file skip/transfer decisions + deletions.
    let (plans, deletions, conflicts) =
        match protocol::read_control(&mut secure, config.limits.max_message_bytes)? {
            Control::SyncResponse {
                files,
                deletions,
                conflicts,
            } => (files, deletions, conflicts),
            other => {
                return Err(Error::protocol(format!(
                    "expected SyncResponse, got {other:?}"
                )))
            }
        };

    let files_total = items.len() as u64;
    let files_transferred_planned = plans
        .iter()
        .filter(|p| !matches!(p.needed, ChunkSelection::None))
        .count() as u64;

    sink.on_event(ProgressEvent::TransferStarted {
        transfer_id: "sync".into(),
        total_files: files_transferred_planned,
        total_bytes: 0,
    });

    let mut bytes_transferred = 0u64;

    if !opts.dry_run {
        // Transfer each changed/new file in file_index order.
        for (idx, item) in items.iter().enumerate() {
            let file_index = idx as u32;
            let plan = plans
                .iter()
                .find(|p| p.file_index == file_index)
                .cloned()
                .unwrap_or(protocol::FilePlan {
                    file_index,
                    needed: ChunkSelection::All,
                    resume: false,
                    delta: false,
                });
            if matches!(plan.needed, ChunkSelection::None) {
                continue; // identical; skipped
            }
            if plan.delta {
                // Changed file with a basis at the destination: delta transfer.
                let sent = send::delta_send_file(
                    config,
                    &mut secure,
                    item,
                    &offers[idx],
                    file_index,
                    sink,
                )?;
                bytes_transferred += sent;
            } else {
                let (sent, _) =
                    send::send_one_file(config, &mut secure, item, &offers[idx], &plan, sink)?;
                bytes_transferred += sent;
            }
        }
    }

    // `Complete` is the sender's last action, mirroring the push protocol: the
    // sender never reads after it, so neither side tears the socket down with a
    // pending read (which Windows turns into an abortive reset). The receiver
    // performs any deletions after reading `Complete`. The reported deletion
    // count is the receiver's plan (what it will remove when `--delete` is set).
    protocol::write_control(&mut secure, &Control::Complete)?;
    secure.graceful_close();

    let deleted = if opts.delete {
        deletions.len() as u64
    } else {
        0
    };

    sink.on_event(ProgressEvent::TransferCompleted {
        transfer_id: "sync".into(),
        bytes_transferred,
        bytes_skipped: 0,
    });

    Ok(SyncSummary {
        dry_run: opts.dry_run,
        files_total,
        files_transferred: files_transferred_planned,
        files_skipped: files_total.saturating_sub(files_transferred_planned),
        bytes_transferred,
        deletions,
        conflicts,
        deleted,
    })
}

/// Walk `source_dir` into [`SendItem`]s whose `rel_path` is relative to the
/// source root (contents-into-destination semantics). Symlinks are not
/// followed.
fn expand_for_sync(source_dir: &Path) -> Result<Vec<SendItem>> {
    let mut items = Vec::new();
    for entry in walkdir::WalkDir::new(source_dir).follow_links(false) {
        let entry = entry.map_err(|e| Error::other(e.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(source_dir)
            .map_err(|e| Error::other(e.to_string()))?;
        items.push(SendItem {
            rel_path: pathsafety::to_wire_rel(rel),
            source_path: entry.path().to_path_buf(),
        });
    }
    Ok(items)
}

/// Whole-file BLAKE3 hash (hex), streamed in bounded memory.
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

/// Random 128-bit id (hex) for a sync session's transient manifest.
fn generate_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}
