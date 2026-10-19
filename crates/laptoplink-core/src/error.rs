//! Error types for the LaptopLink core engine.

use std::path::PathBuf;

/// Result alias used throughout the core engine.
pub type Result<T> = std::result::Result<T, Error>;

/// All errors surfaced by the core engine.
///
/// Variants are intentionally coarse-grained but carry enough context for the
/// CLI to produce actionable messages and stable exit codes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("cryptographic transport error: {0}")]
    Noise(String),

    /// The remote device presented a static key that is not paired/approved.
    #[error("peer not authorized: fingerprint {0} is not paired")]
    PeerNotAuthorized(String),

    /// The remote rejected us (we are not approved on their side).
    #[error("rejected by peer: {0}")]
    RejectedByPeer(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("protocol version mismatch: local {local}, remote {remote}")]
    VersionMismatch { local: u32, remote: u32 },

    /// Integrity check failed (chunk or whole-file hash mismatch).
    #[error("integrity check failed: {0}")]
    Integrity(String),

    /// A destination path escaped the allowed root or was otherwise unsafe.
    #[error("unsafe path rejected: {0}")]
    UnsafePath(String),

    /// A configured resource limit was exceeded.
    #[error("resource limit exceeded: {0}")]
    LimitExceeded(String),

    /// The source file changed underneath an in-flight transfer.
    #[error("source file changed during transfer: {0}")]
    SourceChanged(PathBuf),

    #[error("transfer not found: {0}")]
    TransferNotFound(String),

    #[error("operation cancelled")]
    Cancelled,

    #[error("configuration error: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}

impl From<snow::Error> for Error {
    fn from(e: snow::Error) -> Self {
        Error::Noise(e.to_string())
    }
}
