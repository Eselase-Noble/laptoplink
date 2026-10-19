//! Chunking strategy and content hashing.
//!
//! Files are split into fixed-size chunks (the last chunk may be shorter).
//! Each chunk is independently verifiable by its BLAKE3 hash, which enables
//! partial resume and per-chunk integrity checks. A whole-file BLAKE3 hash is
//! also computed and verified before a received file is committed.

use serde::{Deserialize, Serialize};

/// Default chunk size (4 MiB). Benchmarking (`docs/BENCHMARKING.md`) drives the
/// adaptive policy in [`choose_chunk_size`]; 1–16 MiB is the practical range.
pub const DEFAULT_CHUNK_SIZE: u32 = 4 * 1024 * 1024;
pub const MIN_CHUNK_SIZE: u32 = 64 * 1024; // 64 KiB
pub const MAX_CHUNK_SIZE: u32 = 16 * 1024 * 1024; // 16 MiB

/// Pick a chunk size for a file of `size` bytes.
///
/// Heuristic (to be refined by benchmarks): small files get small chunks to
/// avoid wasted padding/metadata; very large files get larger chunks to amortize
/// per-chunk overhead, capped so that a single in-flight chunk stays bounded.
pub fn choose_chunk_size(size: u64) -> u32 {
    match size {
        0..=1_048_576 => MIN_CHUNK_SIZE.max(256 * 1024).min(size.max(1) as u32),
        1_048_577..=104_857_600 => 1024 * 1024, // <=100 MiB -> 1 MiB
        104_857_601..=10_737_418_240 => DEFAULT_CHUNK_SIZE, // <=10 GiB -> 4 MiB
        _ => 8 * 1024 * 1024,                   // >10 GiB -> 8 MiB
    }
}

/// Number of chunks for `size` bytes at `chunk_size`. Uses 128-bit math to
/// avoid overflow on terabyte-scale inputs.
pub fn chunk_count(size: u64, chunk_size: u32) -> u64 {
    if chunk_size == 0 {
        return 0;
    }
    (size as u128).div_ceil(chunk_size as u128) as u64
}

/// Byte range `[offset, offset+len)` for `chunk_index` of a file.
pub fn chunk_range(chunk_index: u64, chunk_size: u32, file_size: u64) -> (u64, u32) {
    let offset = chunk_index.saturating_mul(chunk_size as u64);
    if offset >= file_size {
        return (file_size, 0);
    }
    let remaining = file_size - offset;
    let len = remaining.min(chunk_size as u64) as u32;
    (offset, len)
}

/// Hash a chunk payload, returning the hex-encoded BLAKE3 digest.
pub fn hash_chunk(data: &[u8]) -> String {
    hex::encode(blake3::hash(data).as_bytes())
}

/// Streaming whole-file hasher wrapper (BLAKE3).
#[derive(Default)]
pub struct FileHasher {
    inner: blake3::Hasher,
}

impl FileHasher {
    pub fn new() -> Self {
        FileHasher {
            inner: blake3::Hasher::new(),
        }
    }
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }
    pub fn finalize_hex(&self) -> String {
        hex::encode(self.inner.finalize().as_bytes())
    }
}

/// Parameters describing how one file is chunked.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ChunkParams {
    pub chunk_size: u32,
    pub file_size: u64,
}

impl ChunkParams {
    pub fn count(&self) -> u64 {
        chunk_count(self.file_size, self.chunk_size)
    }
    pub fn range(&self, index: u64) -> (u64, u32) {
        chunk_range(index, self.chunk_size, self.file_size)
    }
}
