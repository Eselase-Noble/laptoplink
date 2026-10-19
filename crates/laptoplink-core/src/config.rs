//! Configuration, on-disk layout, and the paired-peer store.
//!
//! Layout (via the `directories` crate, cross-platform):
//!   <config_dir>/identity.json   -> this device's static keypair
//!   <config_dir>/peers.json      -> approved peer fingerprints + metadata
//!   <config_dir>/transfers/      -> sender-side transfer manifests
//!   <data_dir>/received/         -> default receive destination

use crate::error::{Error, Result};
use crate::identity::{DeviceIdentity, Fingerprint};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Resolved on-disk locations plus tunable limits.
#[derive(Clone)]
pub struct Config {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub limits: Limits,
}

/// Configurable resource limits. Large files must never imply unbounded
/// resource use; every bound here is enforced somewhere in the engine.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    /// Max bytes buffered in flight on the receive path, per connection.
    pub max_inflight_bytes: usize,
    /// Max number of files in a single transfer.
    pub max_files: u64,
    /// Max length of a single serialized control message (bytes).
    pub max_message_bytes: usize,
    /// Max chunk payload size accepted by the receiver (bytes).
    pub max_chunk_bytes: usize,
    /// Max total manifest entries (chunks across all files).
    pub max_total_chunks: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_inflight_bytes: 64 * 1024 * 1024, // 64 MiB
            max_files: 5_000_000,
            max_message_bytes: 16 * 1024 * 1024, // 16 MiB control message cap
            max_chunk_bytes: 64 * 1024 * 1024,   // 64 MiB hard chunk cap
            max_total_chunks: 1_000_000_000,
        }
    }
}

impl Config {
    /// Resolve the default per-user configuration locations.
    pub fn discover() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("dev", "laptoplink", "laptoplink")
            .ok_or_else(|| Error::Config("cannot determine home directory".into()))?;
        Ok(Config {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
            limits: Limits::default(),
        })
    }

    /// Build a config rooted at an explicit directory (used by tests).
    pub fn at_root(root: &Path) -> Self {
        Config {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            limits: Limits::default(),
        }
    }

    pub fn identity_path(&self) -> PathBuf {
        self.config_dir.join("identity.json")
    }
    pub fn peers_path(&self) -> PathBuf {
        self.config_dir.join("peers.json")
    }
    pub fn transfers_dir(&self) -> PathBuf {
        self.config_dir.join("transfers")
    }
    pub fn default_receive_dir(&self) -> PathBuf {
        self.data_dir.join("received")
    }

    /// Load this device's identity, generating and persisting one on first run.
    pub fn load_or_create_identity(&self, default_name: &str) -> Result<DeviceIdentity> {
        let path = self.identity_path();
        if path.exists() {
            DeviceIdentity::load(&path)
        } else {
            let id = DeviceIdentity::generate(default_name)?;
            id.save(&path)?;
            Ok(id)
        }
    }

    pub fn load_peers(&self) -> Result<PeerStore> {
        PeerStore::load(&self.peers_path())
    }
}

/// A single approved peer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    /// Full 64-char hex fingerprint of the peer's static public key.
    pub fingerprint: String,
    /// Whether this peer is currently allowed to connect / be connected to.
    pub revoked: bool,
}

/// The set of peers this device has explicitly paired with.
///
/// Authorization is based solely on the static public key fingerprint — never
/// on IP address — which prevents spoofing/impersonation by network position.
#[derive(Default, Serialize, Deserialize)]
pub struct PeerStore {
    #[serde(default)]
    peers: BTreeMap<String, Peer>, // keyed by full fingerprint
    #[serde(skip)]
    path: PathBuf,
}

impl PeerStore {
    pub fn load(path: &Path) -> Result<Self> {
        let mut store = if path.exists() {
            let data = std::fs::read(path)?;
            serde_json::from_slice::<PeerStore>(&data)?
        } else {
            PeerStore::default()
        };
        store.path = path.to_path_buf();
        Ok(store)
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(self)?;
        crate::fsutil::write_private_file(&self.path, &data)?;
        Ok(())
    }

    /// Approve (pair) a peer by fingerprint. Idempotent; clears revocation.
    pub fn approve(&mut self, fingerprint: &Fingerprint, name: &str) -> Result<()> {
        self.peers.insert(
            fingerprint.full().to_string(),
            Peer {
                name: name.to_string(),
                fingerprint: fingerprint.full().to_string(),
                revoked: false,
            },
        );
        self.save()
    }

    /// Revoke a peer: keeps the record but denies future connections.
    pub fn revoke(&mut self, fingerprint: &Fingerprint) -> Result<bool> {
        if let Some(p) = self.peers.get_mut(fingerprint.full()) {
            p.revoked = true;
            self.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Whether a peer is paired and not revoked.
    pub fn is_authorized(&self, fingerprint: &Fingerprint) -> bool {
        self.peers
            .get(fingerprint.full())
            .map(|p| !p.revoked)
            .unwrap_or(false)
    }

    pub fn get(&self, fingerprint: &Fingerprint) -> Option<&Peer> {
        self.peers.get(fingerprint.full())
    }

    pub fn list(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values()
    }
}
