//! Durable transfer manifests and the per-file chunk bitmap.
//!
//! Two kinds of record are persisted:
//!
//! * [`ReceiverManifest`] — lives under `<dest>/.laptoplink/<id>.json` and
//!   records, per file, exactly which chunks have been *verified and written*.
//!   It is the source of truth for resume on the receiving side.
//! * [`SenderRecord`] — lives under `<config>/transfers/<id>.json` and lets the
//!   CLI implement `transfer list/status/resume/cancel` on the sending side.
//!
//! Manifests are written atomically (temp file + rename) so a crash mid-write
//! never corrupts them. A chunk is only marked complete *after* its data has
//! been hashed-verified and written to the part file.

use crate::error::Result;
use crate::fsutil;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A compact set of chunk indices backed by a byte-packed bitmap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bitmap {
    /// Number of valid bits (= chunk count).
    len: u64,
    /// Packed bits, hex-encoded on disk.
    #[serde(with = "hex_vec")]
    bits: Vec<u8>,
}

impl Bitmap {
    pub fn new(len: u64) -> Self {
        let bytes = len.div_ceil(8) as usize;
        Bitmap {
            len,
            bits: vec![0u8; bytes],
        }
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn get(&self, i: u64) -> bool {
        if i >= self.len {
            return false;
        }
        let (byte, bit) = ((i / 8) as usize, (i % 8) as u8);
        (self.bits[byte] >> bit) & 1 == 1
    }
    pub fn set(&mut self, i: u64) {
        if i >= self.len {
            return;
        }
        let (byte, bit) = ((i / 8) as usize, (i % 8) as u8);
        self.bits[byte] |= 1 << bit;
    }
    /// Count of set bits (completed chunks).
    pub fn count_set(&self) -> u64 {
        self.bits.iter().map(|b| b.count_ones() as u64).sum()
    }
    pub fn all_set(&self) -> bool {
        self.count_set() == self.len
    }
    /// Indices in `[0, len)` that are *not* set (still needed).
    pub fn missing(&self) -> Vec<u64> {
        (0..self.len).filter(|&i| !self.get(i)).collect()
    }
}

/// Per-file state on the receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverFile {
    pub file_index: u32,
    pub rel_path: String,
    pub size: u64,
    pub chunk_size: u32,
    /// Source mtime from the offer; part of the resume validity signature.
    pub src_mtime: Option<u64>,
    /// Expected whole-file hash, learned when the sender finishes the file.
    pub expected_file_hash: Option<String>,
    /// Which chunks have been verified + written.
    pub verified: Bitmap,
    /// Path to the in-progress `.part` file.
    pub part_path: PathBuf,
    /// True once the file has been verified and atomically committed.
    pub committed: bool,
}

/// The receiver's durable manifest for one transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverManifest {
    pub transfer_id: String,
    pub protocol_version: u32,
    pub peer_fingerprint: String,
    pub root: PathBuf,
    pub files: Vec<ReceiverFile>,
    #[serde(skip)]
    path: PathBuf,
}

impl ReceiverManifest {
    pub fn manifest_path(root: &Path, transfer_id: &str) -> PathBuf {
        root.join(".laptoplink").join(format!("{transfer_id}.json"))
    }

    pub fn load(root: &Path, transfer_id: &str) -> Result<Option<Self>> {
        let path = Self::manifest_path(root, transfer_id);
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read(&path)?;
        let mut m: ReceiverManifest = serde_json::from_slice(&data)?;
        m.path = path;
        Ok(Some(m))
    }

    pub fn new(root: &Path, transfer_id: &str, peer_fingerprint: &str) -> Self {
        ReceiverManifest {
            transfer_id: transfer_id.to_string(),
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            peer_fingerprint: peer_fingerprint.to_string(),
            root: root.to_path_buf(),
            files: Vec::new(),
            path: Self::manifest_path(root, transfer_id),
        }
    }

    /// Persist atomically: write to `<path>.tmp`, fsync, rename over `<path>`.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self)?;
        // write_private_file fsyncs on its write handle (durable before rename).
        fsutil::write_private_file(&tmp, &data)?;
        fsutil::atomic_rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn file_mut(&mut self, file_index: u32) -> Option<&mut ReceiverFile> {
        self.files.iter_mut().find(|f| f.file_index == file_index)
    }
}

/// Lightweight sender-side record for `transfer list/status/resume/cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderRecord {
    pub transfer_id: String,
    pub target_addr: String,
    pub source_root: PathBuf,
    pub files: Vec<SenderFile>,
    pub state: TransferState,
    /// Unix timestamp (seconds) when the transfer was created.
    pub created_unix: u64,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderFile {
    pub rel_path: String,
    pub source_path: PathBuf,
    pub size: u64,
    pub chunk_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

impl SenderRecord {
    /// Build a new in-memory record (not yet persisted).
    pub fn new(
        transfer_id: String,
        target_addr: String,
        source_root: PathBuf,
        files: Vec<SenderFile>,
        created_unix: u64,
    ) -> Self {
        SenderRecord {
            transfer_id,
            target_addr,
            source_root,
            files,
            state: TransferState::Pending,
            created_unix,
            path: PathBuf::new(),
        }
    }

    pub fn record_path(transfers_dir: &Path, transfer_id: &str) -> PathBuf {
        transfers_dir.join(format!("{transfer_id}.json"))
    }

    pub fn save_to(&mut self, transfers_dir: &Path) -> Result<()> {
        self.path = Self::record_path(transfers_dir, &self.transfer_id);
        std::fs::create_dir_all(transfers_dir)?;
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self)?;
        fsutil::write_private_file(&tmp, &data)?;
        fsutil::atomic_rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn load(transfers_dir: &Path, transfer_id: &str) -> Result<Option<Self>> {
        let path = Self::record_path(transfers_dir, transfer_id);
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read(&path)?;
        let mut r: SenderRecord = serde_json::from_slice(&data)?;
        r.path = path;
        Ok(Some(r))
    }

    pub fn list(transfers_dir: &Path) -> Result<Vec<SenderRecord>> {
        let mut out = Vec::new();
        if !transfers_dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(transfers_dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(data) = std::fs::read(&p) {
                    if let Ok(mut r) = serde_json::from_slice::<SenderRecord>(&data) {
                        r.path = p;
                        out.push(r);
                    }
                }
            }
        }
        out.sort_by_key(|r| r.created_unix);
        Ok(out)
    }

    pub fn set_state_and_save(&mut self, state: TransferState) -> Result<()> {
        self.state = state;
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self)?;
        fsutil::write_private_file(&tmp, &data)?;
        fsutil::atomic_rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Current wall-clock time in whole seconds since the Unix epoch.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Serde helper to store `Vec<u8>` as hex.
mod hex_vec {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_set_get_count() {
        let mut b = Bitmap::new(20);
        assert_eq!(b.count_set(), 0);
        b.set(0);
        b.set(7);
        b.set(19);
        assert!(b.get(0) && b.get(7) && b.get(19));
        assert!(!b.get(1));
        assert_eq!(b.count_set(), 3);
        assert_eq!(b.missing().len(), 17);
        assert!(!b.all_set());
    }
}
