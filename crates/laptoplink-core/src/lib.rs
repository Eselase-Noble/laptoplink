//! # laptoplink-core
//!
//! The reusable transfer engine shared by every LaptopLink interface (CLI,
//! desktop, web backend). It owns device identity, the secure transport, the
//! wire protocol, chunking + integrity, durable manifests, and the send/receive
//! state machines.
//!
//! ## Design goals (Phase 2)
//!
//! * **Security first** — mutually-authenticated Noise handshake, AEAD on every
//!   byte, authorization by static-key fingerprint (never by IP).
//! * **Integrity always** — per-chunk and whole-file BLAKE3 verification; a
//!   received file is only committed after it fully verifies.
//! * **Bounded memory** — streaming I/O with a single in-flight chunk buffer;
//!   memory use is independent of file size.
//! * **Recoverable** — durable manifests + atomic finalize allow resume after
//!   crashes, disconnects, and restarts.
//!
//! The engine uses blocking `std::net` I/O with one thread per connection in
//! this phase; the async swap is isolated behind [`transport`].
//!
//! ```no_run
//! use laptoplink_core::{config::Config, Engine};
//! let cfg = Config::discover().unwrap();
//! let engine = Engine::new(cfg, "my-laptop").unwrap();
//! println!("device fingerprint: {}", engine.identity().fingerprint());
//! ```

pub mod chunk;
pub mod config;
pub mod error;
pub mod fsutil;
pub mod identity;
pub mod manifest;
pub mod pathsafety;
pub mod progress;
pub mod protocol;
pub mod recv;
pub mod send;
pub mod transport;

pub use error::{Error, Result};

/// The Noise handshake pattern and primitives used for all transport.
///
/// `XX` gives mutual authentication while transmitting both static keys during
/// the handshake, which pairs naturally with our fingerprint-based trust model.
/// ChaChaPoly is the AEAD; BLAKE2s the handshake hash.
pub const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

use config::Config;
use identity::DeviceIdentity;

/// A convenience handle bundling resolved configuration with this device's
/// identity. Interfaces construct one of these and call into [`send`]/[`recv`].
pub struct Engine {
    config: Config,
    identity: DeviceIdentity,
}

impl Engine {
    /// Build an engine, loading or creating the device identity on first use.
    pub fn new(config: Config, default_name: &str) -> Result<Self> {
        let identity = config.load_or_create_identity(default_name)?;
        Ok(Engine { config, identity })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }
}
