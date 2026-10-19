//! Cryptographic device identity.
//!
//! A device identity is a long-lived X25519 static keypair used by the Noise
//! handshake. The public key *is* the device's identity; its fingerprint is a
//! BLAKE3 hash rendered as hex, used for display and pairing decisions.
//!
//! Private keys are stored on disk with `0o600` permissions on Unix. For a
//! production build these should move into the OS keystore (Keychain / DPAPI /
//! Secret Service); that is tracked as remaining work in `docs/FEATURES.md`.

use crate::error::{Error, Result};
use crate::NOISE_PARAMS;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A device's long-lived identity (static Noise keypair).
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// Human-friendly name advertised to peers (not security-relevant).
    pub name: String,
    /// X25519 public key (32 bytes), hex-encoded on disk.
    #[serde(with = "hex_bytes")]
    pub public_key: Vec<u8>,
    /// X25519 private key (32 bytes), hex-encoded on disk.
    #[serde(with = "hex_bytes")]
    private_key: Vec<u8>,
}

impl DeviceIdentity {
    /// Generate a fresh identity with the given display name.
    pub fn generate(name: impl Into<String>) -> Result<Self> {
        let builder = snow::Builder::new(NOISE_PARAMS.parse().expect("valid noise params"));
        let kp = builder.generate_keypair()?;
        Ok(DeviceIdentity {
            name: name.into(),
            public_key: kp.public,
            private_key: kp.private,
        })
    }

    /// Borrow the raw private key bytes (used only to build Noise states).
    pub(crate) fn private_key(&self) -> &[u8] {
        &self.private_key
    }

    /// The device fingerprint: `hex(blake3(public_key))`, truncated display.
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of_public_key(&self.public_key)
    }

    /// Load an identity from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)?;
        Ok(serde_json::from_slice(&data)?)
    }

    /// Persist the identity to `path` with restrictive permissions.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(self)?;
        crate::fsutil::write_private_file(path, &data)?;
        Ok(())
    }
}

/// A device fingerprint derived from a public key.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Fingerprint(String);

impl Fingerprint {
    pub fn of_public_key(public_key: &[u8]) -> Self {
        let hash = blake3::hash(public_key);
        Fingerprint(hex::encode(hash.as_bytes()))
    }

    /// Full 64-char hex fingerprint.
    pub fn full(&self) -> &str {
        &self.0
    }

    /// Short 16-char fingerprint for display/pairing confirmation.
    pub fn short(&self) -> String {
        self.0[..16].to_string()
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for Fingerprint {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim().to_lowercase();
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Config(format!("invalid fingerprint: {s}")));
        }
        Ok(Fingerprint(s))
    }
}

/// Serde helper to store byte vectors as hex strings.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(&s).map_err(serde::de::Error::custom)
    }
}
